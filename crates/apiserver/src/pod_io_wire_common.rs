//! Shared wire-test harness for the T4.2 Scope C pod_io relay.
//!
//! Test-only module (`#[cfg(test)]`): drives the REAL axum router
//! in-process (`tower::ServiceExt::oneshot`, binding.rs pattern) over a
//! shared `EmbeddedStorage`, plus a canned kubelet TCP listener on an
//! ephemeral loopback port - no external processes, no network beyond
//! 127.0.0.1. This is the seam behind decision Q31 (plain-HTTP single-shot
//! logs/exec relay), exercised end to end by golden G30.
//!
//! Exists as its own file because `pod_io_wire_tests.rs` hit the 400-line
//! cap and was split by responsibility: happy-path relay tests stay in
//! `pod_io_wire_tests.rs`, pre-hop resolution/validation branches live in
//! `pod_io_wire_resolution_tests.rs`, and every helper those two share is
//! hoisted here as `pub(crate)`.

use std::sync::{Arc, Mutex};

use api::SchemaRegistry;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, StorageBackend};
use tower::ServiceExt;

use crate::app;
use crate::state::AppState;

pub(crate) fn app(store: Arc<EmbeddedStorage>) -> axum::Router {
    let any: Arc<dyn StorageBackend> = store;
    app::router(AppState {
        registry: Arc::new(SchemaRegistry::with_core_v1()),
        store: any,
        server_address: "127.0.0.1:6443".into(),
    })
}

/// Send one request, return `(status, raw lossy-UTF8 body, content-type)`.
pub(crate) async fn send(
    store: Arc<EmbeddedStorage>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String, Option<String>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(payload) = &body {
        builder = builder.header("content-type", "application/json");
        let req = builder.body(Body::from(payload.to_string())).unwrap();
        let resp = app(store).oneshot(req).await.unwrap();
        return head_and_text(resp).await;
    }
    let resp = app(store)
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    head_and_text(resp).await
}

/// Split a response into status + lossy-UTF8 text + lowercase-none CT.
pub(crate) async fn head_and_text(
    resp: axum::response::Response,
) -> (StatusCode, String, Option<String>) {
    let code = resp.status();
    let ctype = resp
        .headers()
        .get("content-type")
        .map(|value| value.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, String::from_utf8_lossy(&bytes).to_string(), ctype)
}

/// The same exchange with the body decoded as a JSON Status/List value.
pub(crate) async fn do_request(
    store: Arc<EmbeddedStorage>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, Option<String>) {
    let (code, text, ctype) = send(store, method, uri, body).await;
    (
        code,
        serde_json::from_str(&text).unwrap_or(Value::Null),
        ctype,
    )
}

pub(crate) async fn seed(store: &EmbeddedStorage, resource: &str, ns: &str, name: &str, v: Value) {
    store
        .create(&Key::new("", resource, ns, name), v)
        .await
        .unwrap();
}

pub(crate) fn pod(uid: &str, node_name: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "web", "namespace": "default", "uid": uid},
        "spec": {"nodeName": node_name, "containers": [{"name": "app", "image": "img"}]},
    })
}

pub(crate) fn node(ip: Option<&str>, port: Option<u16>) -> Value {
    let mut annotations = serde_json::Map::new();
    if let Some(p) = port {
        annotations.insert("init-pro.io/kubelet-port".into(), json!(p.to_string()));
    }
    let addresses = match ip {
        Some(ip) => {
            json!([{"type": "Hostname", "address": "h"}, {"type": "InternalIP", "address": ip}])
        }
        None => Value::Null,
    };
    json!({
        "apiVersion": "v1", "kind": "Node",
        "metadata": {"name": "n1", "annotations": annotations},
        "status": {"addresses": addresses},
    })
}

pub(crate) async fn seeded_pod_on_node(store: Arc<EmbeddedStorage>, node_json: Value) {
    seed(&store, "pods", "default", "web", pod("uid-1", "n1")).await;
    seed(&store, "nodes", "", "n1", node_json).await;
}

/// Serve one canned response to exactly one connection; capture the request.
/// Stops reading as soon as head + Content-Length body has arrived (the
/// relay client keeps its write side open while awaiting the answer).
pub(crate) async fn serve_once(response: String) -> (u16, Arc<Mutex<Vec<u8>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0u8; 8192];
        loop {
            let Ok(n) = sock.read(&mut buf).await else {
                return;
            };
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
            let seen = sink.lock().unwrap().clone();
            if let Some(p) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                let body_len: usize = String::from_utf8_lossy(&seen[..p])
                    .to_ascii_lowercase()
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if seen.len() >= p + 4 + body_len {
                    break;
                }
            }
        }
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
        drop(sock);
    });
    (port, captured)
}

pub(crate) fn http_response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
