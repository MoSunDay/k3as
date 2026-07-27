//! T1.2b watch stream: real-TCP round-trip over the embedded store.
//!
//! Opens `GET /<collection>?watch=1`, then drives CRUD from a second connection
//! and asserts the `ADDED`/`DELETED` event sequence arrives on the watch
//! stream (chunked `application/json`, one JSON object per line). This mirrors
//! the router's `phase_chain.rs` minimal-HTTP-client pattern (no new deps).

use std::sync::Arc;
use std::time::Duration;

use api::SchemaRegistry;
use serde_json::Value;
use storage::{EmbeddedStorage, StorageBackend};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn served() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    api::initpro::register(&mut reg);
    reg
}

/// Spawn the full api_app over a fresh embedded store on an ephemeral port.
async fn spawn_server() -> std::net::SocketAddr {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let app = apiserver::api_app(Arc::new(served()), store, "127.0.0.1:6443".to_string());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // give the accept loop a moment to arm
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// One-shot HTTP/1.1 request over a fresh connection (connection: close).
async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: Option<&[u8]>,
) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nconnection: close\r\n");
    if let Some(ct) = content_type {
        req.push_str(&format!("content-type: {ct}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("content-length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    if let Some(b) = body {
        s.write_all(b).await.unwrap();
    }
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    let status: u16 = text.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
    (status, buf[body_start.min(buf.len())..].to_vec())
}

/// Extract complete watch-event JSON objects out of a (chunked) byte buffer.
/// Each event is a single `{"type":...,"object":...}` line; chunk-size hex and
/// the CRLF framing are skipped because they don't parse as JSON objects.
fn extract_events(buf: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(buf);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                if v.get("type").is_some() {
                    out.push(v);
                }
            }
        }
    }
    out
}

#[tokio::test(flavor = "current_thread")]
async fn watch_streams_added_then_deleted() {
    let addr = spawn_server().await;

    // Seed one object before the watch opens (the embedded backend serves live
    // events from subscription, so this one is NOT replayed).
    let cm0 = br#"{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"seed","namespace":"default"},"data":{}}"#;
    let (st, _) = http(addr, "POST", "/api/v1/namespaces/default/configmaps", Some("application/json"), Some(cm0)).await;
    assert_eq!(st, 201);

    // Open the watch stream and keep the socket.
    let mut watch = TcpStream::connect(addr).await.unwrap();
    watch
        .write_all(b"GET /api/v1/namespaces/default/configmaps?watch=1 HTTP/1.1\r\nHost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create + delete a second object from a separate connection.
    let cm1 = br#"{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"live","namespace":"default"},"data":{"k":"v"}}"#;
    let (st, _) = http(addr, "POST", "/api/v1/namespaces/default/configmaps", Some("application/json"), Some(cm1)).await;
    assert_eq!(st, 201);
    let (st, _) = http(addr, "DELETE", "/api/v1/namespaces/default/configmaps/live", None, None).await;
    assert_eq!(st, 200);

    // Drain the watch stream until we observe ADDED(live) then DELETED(live).
    let mut buf = Vec::new();
    let mut tmp = [0u8; 512];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let got = tokio::time::timeout_at(deadline, watch.read(&mut tmp)).await;
        match got {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break, // timeout
        }
        let events = extract_events(&buf);
        let added = events.iter().any(|e| {
            e["type"] == "ADDED" && e["object"]["metadata"]["name"] == "live"
        });
        let deleted = events.iter().any(|e| {
            e["type"] == "DELETED" && e["object"]["metadata"]["name"] == "live"
        });
        if added && deleted {
            let added_evt = events.iter().find(|e| e["type"] == "ADDED").unwrap();
            assert_eq!(added_evt["object"]["kind"], "ConfigMap");
            assert!(added_evt["object"]["metadata"]["resourceVersion"].as_str().is_some());
            assert_eq!(added_evt["object"]["data"]["k"], "v");
            let del_evt = events.iter().find(|e| e["type"] == "DELETED").unwrap();
            assert!(del_evt["object"]["metadata"]["resourceVersion"].as_str().is_some());
            return; // success
        }
    }
    panic!("did not observe ADDED+DELETED for 'live'; got buffer:\n{}", String::from_utf8_lossy(&buf));
}
