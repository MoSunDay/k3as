//! T1.2b watch stream: real-TCP round-trip over the embedded store.
//!
//! Opens `GET /<collection>?watch=1`, then drives CRUD from a second connection
//! and asserts the `ADDED`/`DELETED` event sequence arrives on the watch
//! stream (chunked `application/json`, one JSON object per line). This mirrors
//! the router's `phase_chain.rs` minimal-HTTP-client pattern (no new deps).
//!
//! Also covers the `resourceVersion` contract: `0` replays retained history
//! then continues live, `N` starts strictly after N, compacted history maps
//! to 410 Gone (`Expired`), and `DELETED` events carry the final object.

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

/// Spawn the full api_app over a caller-supplied store on an ephemeral port
/// (tests that need to poke the backend directly, e.g. compaction, keep the
/// `Arc` handle).
async fn spawn_server_with(store: Arc<dyn StorageBackend>) -> std::net::SocketAddr {
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

/// Spawn the full api_app over a fresh embedded store on an ephemeral port.
async fn spawn_server() -> std::net::SocketAddr {
    spawn_server_with(Arc::new(EmbeddedStorage::new())).await
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
    let status: u16 = text
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
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
    let (st, _) = http(
        addr,
        "POST",
        "/api/v1/namespaces/default/configmaps",
        Some("application/json"),
        Some(cm0),
    )
    .await;
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
    let (st, _) = http(
        addr,
        "POST",
        "/api/v1/namespaces/default/configmaps",
        Some("application/json"),
        Some(cm1),
    )
    .await;
    assert_eq!(st, 201);
    let (st, _) = http(
        addr,
        "DELETE",
        "/api/v1/namespaces/default/configmaps/live",
        None,
        None,
    )
    .await;
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
        let added = events
            .iter()
            .any(|e| e["type"] == "ADDED" && e["object"]["metadata"]["name"] == "live");
        let deleted = events
            .iter()
            .any(|e| e["type"] == "DELETED" && e["object"]["metadata"]["name"] == "live");
        if added && deleted {
            let added_evt = events.iter().find(|e| e["type"] == "ADDED").unwrap();
            assert_eq!(added_evt["object"]["kind"], "ConfigMap");
            assert!(added_evt["object"]["metadata"]["resourceVersion"]
                .as_str()
                .is_some());
            assert_eq!(added_evt["object"]["data"]["k"], "v");
            let del_evt = events.iter().find(|e| e["type"] == "DELETED").unwrap();
            assert!(del_evt["object"]["metadata"]["resourceVersion"]
                .as_str()
                .is_some());
            return; // success
        }
    }
    panic!(
        "did not observe ADDED+DELETED for 'live'; got buffer:\n{}",
        String::from_utf8_lossy(&buf)
    );
}

// ---------------------------------------------------------------------------
// resourceVersion semantics (T1.2b): replay (rv=0), resume-after-N (rv=N),
// 410 Expired on compaction, and DELETED carrying the final object.
// ---------------------------------------------------------------------------

/// POST a minimal ConfigMap `{"k":"<data>"}`; assert 201, return the body.
async fn post_configmap(addr: std::net::SocketAddr, name: &str, data_value: &str) -> Value {
    let body = format!(
        r#"{{"apiVersion":"v1","kind":"ConfigMap","metadata":{{"name":"{name}","namespace":"default"}},"data":{{"k":"{data_value}"}}}}"#
    );
    let (st, out) = http(
        addr,
        "POST",
        "/api/v1/namespaces/default/configmaps",
        Some("application/json"),
        Some(body.as_bytes()),
    )
    .await;
    assert_eq!(
        st,
        201,
        "POST {name} -> {st}: {}",
        String::from_utf8_lossy(&out)
    );
    serde_json::from_slice(&out).unwrap()
}

/// Open a raw watch stream (`?watch=1<query>`) and keep the socket open.
async fn open_watch(addr: std::net::SocketAddr, query: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET /api/v1/namespaces/default/configmaps?watch=1{query} HTTP/1.1\r\n\
         Host: localhost\r\nconnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    // let the server subscribe before the test drives more traffic
    tokio::time::sleep(Duration::from_millis(50)).await;
    s
}

/// Read `watch` into `buf` until `pred` holds over the parsed events or the
/// deadline passes; returns every event seen so far (mirrors the live test's
/// read-until-timeout loop).
async fn read_watch_until(
    watch: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: tokio::time::Instant,
    pred: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let mut tmp = [0u8; 512];
    loop {
        let events = extract_events(buf);
        if !events.is_empty() && pred(&events) {
            return events;
        }
        match tokio::time::timeout_at(deadline, watch.read(&mut tmp)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return extract_events(buf),
            Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => return extract_events(buf), // timeout
        }
    }
}

/// `resourceVersion=0` replays retained history, then the same stream keeps
/// delivering live events (the replay -> live seam) over real HTTP.
#[tokio::test(flavor = "current_thread")]
async fn watch_resource_version_zero_replays_history_then_live() {
    let addr = spawn_server().await;
    post_configmap(addr, "alpha", "v").await;
    post_configmap(addr, "beta", "v").await;

    let mut watch = open_watch(addr, "&resourceVersion=0").await;
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    // Replay: both pre-existing objects must arrive as ADDED, in write order.
    let events = read_watch_until(&mut watch, &mut buf, deadline, |evs| {
        evs.iter().filter(|e| e["type"] == "ADDED").count() >= 2
    })
    .await;
    let added: Vec<&Value> = events.iter().filter(|e| e["type"] == "ADDED").collect();
    assert_eq!(
        added.len(),
        2,
        "expected only alpha+beta replay, got: {events:?}"
    );
    for (e, name) in added.iter().zip(["alpha", "beta"]) {
        assert_eq!(e["object"]["metadata"]["name"], name);
        assert_eq!(e["object"]["kind"], "ConfigMap");
        assert!(e["object"]["metadata"]["resourceVersion"]
            .as_str()
            .is_some());
    }

    // Live seam: a create after the watch opened flows on the same stream.
    post_configmap(addr, "gamma", "v").await;
    let events = read_watch_until(&mut watch, &mut buf, deadline, |evs| {
        evs.iter()
            .any(|e| e["type"] == "ADDED" && e["object"]["metadata"]["name"] == "gamma")
    })
    .await;
    let gamma = events
        .iter()
        .find(|e| e["type"] == "ADDED" && e["object"]["metadata"]["name"] == "gamma")
        .expect("gamma ADDED after replay");
    assert_eq!(gamma["object"]["kind"], "ConfigMap");
    assert!(gamma["object"]["metadata"]["resourceVersion"]
        .as_str()
        .is_some());
}

/// `resourceVersion=N` = "events AFTER N": a watch resuming at alpha's rv=1
/// must yield beta only, never a replay of alpha itself.
#[tokio::test(flavor = "current_thread")]
async fn watch_resource_version_n_starts_after_n() {
    let addr = spawn_server().await;
    let alpha = post_configmap(addr, "alpha", "v").await;
    let rv = alpha["metadata"]["resourceVersion"].as_str().unwrap();
    assert_eq!(rv, "1");
    post_configmap(addr, "beta", "v").await;

    let mut watch = open_watch(addr, "&resourceVersion=1").await;
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let events = read_watch_until(&mut watch, &mut buf, deadline, |evs| !evs.is_empty()).await;

    assert!(
        !events.is_empty(),
        "watch after rv=1 produced no events; buffer:\n{}",
        String::from_utf8_lossy(&buf)
    );
    for e in &events {
        assert_eq!(e["type"], "ADDED");
        assert_eq!(
            e["object"]["metadata"]["name"], "beta",
            "events after revision 1 must not replay alpha: {events:?}"
        );
    }
    assert_eq!(
        events.len(),
        1,
        "expected exactly the beta event: {events:?}"
    );
}

/// A watch start at or below the compaction watermark is 410 Gone with
/// Status reason `Expired` (k8s "too old resource version" contract).
#[tokio::test(flavor = "current_thread")]
async fn watch_too_old_resource_version_returns_410_expired() {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let addr = spawn_server_with(store.clone()).await;
    post_configmap(addr, "alpha", "v").await;
    post_configmap(addr, "beta", "v").await; // cluster now at revision 2

    // Drop history up to revision 2: rv=0 replay (start=1) is now too old.
    store.compact(2).await.unwrap();

    let (st, body) = http(
        addr,
        "GET",
        "/api/v1/namespaces/default/configmaps?watch=1&resourceVersion=0",
        None,
        None,
    )
    .await;
    assert_eq!(st, 410, "body: {}", String::from_utf8_lossy(&body));
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["reason"], "Expired");
    assert_eq!(v["code"], 410);
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("too old resource version"),
        "message: {}",
        v["message"]
    );
}

/// A DELETED event carries the object's final full state (post-replace, not
/// the create-time value) with resourceVersion = the deletion revision.
#[tokio::test(flavor = "current_thread")]
async fn deleted_watch_event_carries_final_object() {
    let addr = spawn_server().await;
    let created = post_configmap(addr, "delta", "final").await;
    let rv = created["metadata"]["resourceVersion"].as_str().unwrap();

    // Full-object replace (rv CAS) bumps delta to revision 2 with new data.
    let replace = format!(
        r#"{{"apiVersion":"v1","kind":"ConfigMap","metadata":{{"name":"delta","namespace":"default","resourceVersion":"{rv}"}},"data":{{"k":"final2"}}}}"#
    );
    let (st, out) = http(
        addr,
        "PUT",
        "/api/v1/namespaces/default/configmaps/delta",
        Some("application/json"),
        Some(replace.as_bytes()),
    )
    .await;
    assert_eq!(st, 200, "body: {}", String::from_utf8_lossy(&out));
    assert_eq!(
        serde_json::from_slice::<Value>(&out).unwrap()["metadata"]["resourceVersion"],
        "2"
    );

    // Live watch, then delete (revision 3) from a second connection.
    let mut watch = open_watch(addr, "").await;
    let (st, _) = http(
        addr,
        "DELETE",
        "/api/v1/namespaces/default/configmaps/delta",
        None,
        None,
    )
    .await;
    assert_eq!(st, 200);

    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let events = read_watch_until(&mut watch, &mut buf, deadline, |evs| {
        evs.iter()
            .any(|e| e["type"] == "DELETED" && e["object"]["metadata"]["name"] == "delta")
    })
    .await;
    let del = events
        .iter()
        .find(|e| e["type"] == "DELETED" && e["object"]["metadata"]["name"] == "delta")
        .unwrap_or_else(|| {
            panic!(
                "no DELETED for delta; buffer:\n{}",
                String::from_utf8_lossy(&buf)
            )
        });
    assert_eq!(del["object"]["data"]["k"], "final2"); // final state, not "final"
    assert_eq!(del["object"]["metadata"]["resourceVersion"], "3"); // create=1 replace=2 delete=3
}
