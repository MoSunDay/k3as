//! Bounded graceful-drain regression (TODO **T4.2**, the G25 teardown hang).
//!
//! `axum::serve(...).with_graceful_shutdown` waits for ALL in-flight
//! requests, but watch streams (kubelet informers) are infinite — with one
//! connected watcher the server stayed "draining" forever on SIGTERM.
//! [`apiserver::serve`] now bounds the drain (`DRAIN_GRACE`) so the process
//! meets the 5s SIGTERM-exit contract of `scripts/graceful-shutdown-test.sh`.

use std::sync::Arc;
use std::time::Duration;

use api::SchemaRegistry;
use storage::{EmbeddedStorage, StorageBackend};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn serve_returns_with_an_open_watch_stream_after_shutdown() {
    // Pick a free port: bind the OS-chosen one, read it, release it.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let handle = tokio::spawn(apiserver::serve(
        SchemaRegistry::with_core_v1(),
        store,
        addr,
        format!("127.0.0.1:{port}"),
        async move {
            let _ = rx.await;
        },
    ));

    // Open a raw watch stream and read a few bytes so it is established
    // (and stays open: watch never completes on its own). Retry the connect
    // briefly: the spawned serve task binds asynchronously.
    let mut client = None;
    for _ in 0..100 {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => {
                client = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let mut client = client.expect("server came up");
    client
        .write_all(b"GET /api/v1/pods?watch=1 HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 128];
    let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
        .await
        .expect("watch stream answered before the deadline")
        .expect("watch stream readable");
    assert!(n > 0, "watch stream produced response bytes");

    // Fire shutdown: serve must return even though the watch never ends.
    let _ = tx.send(());
    let res = tokio::time::timeout(Duration::from_secs(4), handle)
        .await
        .expect("serve exited within the drain deadline")
        .expect("serve task did not panic");
    assert!(res.is_ok(), "serve drained cleanly: {res:?}");

    drop(client);
}
