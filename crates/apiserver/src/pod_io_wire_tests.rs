//! Router-level wire tests for the T4.2 Scope C relay.
//!
//! Drives the REAL axum router in-process (`tower::ServiceExt::oneshot`,
//! binding.rs pattern) over a shared `EmbeddedStorage`, with a canned
//! kubelet TCP listener on an ephemeral loopback port — no external
//! processes, no network beyond 127.0.0.1.

use std::sync::{Arc, Mutex};

use api::SchemaRegistry;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, StorageBackend};
use tower::ServiceExt;

use crate::app;
use crate::state::AppState;

fn app(store: Arc<EmbeddedStorage>) -> axum::Router {
    let any: Arc<dyn StorageBackend> = store;
    app::router(AppState {
        registry: Arc::new(SchemaRegistry::with_core_v1()),
        store: any,
        server_address: "127.0.0.1:6443".into(),
    })
}

/// Send one request, return `(status, raw lossy-UTF8 body, content-type)`.
async fn send(
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
async fn head_and_text(resp: axum::response::Response) -> (StatusCode, String, Option<String>) {
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
async fn do_request(
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

async fn seed(store: &EmbeddedStorage, resource: &str, ns: &str, name: &str, v: Value) {
    store
        .create(&Key::new("", resource, ns, name), v)
        .await
        .unwrap();
}

fn pod(uid: &str, node_name: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "web", "namespace": "default", "uid": uid},
        "spec": {"nodeName": node_name, "containers": [{"name": "app", "image": "img"}]},
    })
}

fn node(ip: Option<&str>, port: Option<u16>) -> Value {
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

async fn seeded_pod_on_node(store: Arc<EmbeddedStorage>, node_json: Value) {
    seed(&store, "pods", "default", "web", pod("uid-1", "n1")).await;
    seed(&store, "nodes", "", "n1", node_json).await;
}

/// Serve one canned response to exactly one connection; capture the request.
/// Stops reading as soon as head + Content-Length body has arrived (the
/// relay client keeps its write side open while awaiting the answer).
async fn serve_once(response: String) -> (u16, Arc<Mutex<Vec<u8>>>) {
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

fn http_response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

// ------------------------------------------------------- resolution branches

#[tokio::test]
async fn log_unknown_pod_is_404_status() {
    let store = Arc::new(EmbeddedStorage::new());
    let (code, status, _) = do_request(
        store,
        "GET",
        "/api/v1/namespaces/default/pods/web/log",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(status["kind"], "Status");
    assert_eq!(status["apiVersion"], "v1");
    assert_eq!(status["status"], "Failure");
    assert_eq!(status["message"], "pods \"web\" not found");
    assert_eq!(status["code"], 404);
}

#[tokio::test]
async fn unscheduled_pod_is_404_with_exact_message() {
    let store = Arc::new(EmbeddedStorage::new());
    seed(&store, "pods", "default", "web", pod("uid-1", "")).await;
    let (code, status, ..) = do_request(
        store,
        "GET",
        "/api/v1/namespaces/default/pods/web/log",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(status["message"], "pod not yet scheduled to a node");
    assert_eq!(status["code"], 404);
}

#[tokio::test]
async fn missing_node_object_is_404() {
    let store = Arc::new(EmbeddedStorage::new());
    seed(&store, "pods", "default", "web", pod("uid-1", "gone")).await;
    let (code, status, _) = do_request(
        store,
        "GET",
        "/api/v1/namespaces/default/pods/web/log",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(status["message"], "nodes \"gone\" not found");
}

#[tokio::test]
async fn node_without_internal_ip_is_502() {
    let store = Arc::new(EmbeddedStorage::new());
    seeded_pod_on_node(store.clone(), node(None, None)).await;
    let (code, status, ..) = do_request(
        store,
        "GET",
        "/api/v1/namespaces/default/pods/web/log?container=app",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert_eq!(status["message"], "node \"n1\" has no InternalIP address");
    assert_eq!(status["code"], 502);
}

#[tokio::test]
async fn pod_without_uid_is_404_before_any_hop() {
    let store = Arc::new(EmbeddedStorage::new());
    seed(
        &store,
        "pods",
        "default",
        "web",
        json!({"metadata": {"name": "web"}, "spec": {"nodeName": "n1"}}),
    )
    .await;
    seed(&store, "nodes", "", "n1", node(Some("127.0.0.1"), Some(1))).await;
    let (code, status, _) = do_request(
        store,
        "GET",
        "/api/v1/namespaces/default/pods/web/log",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(status["message"], "pod has no uid");
}

#[tokio::test]
async fn bad_tail_lines_is_400_without_storage_access() {
    let store = Arc::new(EmbeddedStorage::new()); // nothing seeded at all
    let (code, status, ..) = do_request(
        store,
        "GET",
        "/api/v1/namespaces/default/pods/web/log?tailLines=-3",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(status["message"].as_str().unwrap().contains("tailLines"));
}

#[tokio::test]
async fn bad_exec_body_is_400_before_resolution() {
    let store = Arc::new(EmbeddedStorage::new()); // nothing seeded at all
    for payload in [
        json!({"command": []}),
        json!({"command": ["a", 2]}),
        json!("strings"),
    ] {
        let (code, status, ..) = do_request(
            store.clone(),
            "POST",
            "/api/v1/namespaces/default/pods/web/exec",
            Some(payload),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{status}");
    }
}

// --------------------------------------------------------------- happy paths

#[tokio::test]
async fn log_relays_bytes_verbatim_and_hits_the_kubelet_path() {
    let (port, captured) =
        serve_once(http_response("200 OK", "text/plain", "hello\nworld\n")).await;
    let store = Arc::new(EmbeddedStorage::new());
    seeded_pod_on_node(store.clone(), node(Some("127.0.0.1"), Some(port))).await;

    let (code, text, ctype) = send(
        store,
        "GET",
        "/api/v1/namespaces/default/pods/web/log?container=app&tailLines=3",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("text/plain"));
    assert_eq!(text, "hello\nworld\n"); // raw passthrough, byte-for-byte

    let request = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(
        request.starts_with("GET /pods/uid-1/containers/app/log?tailLines=3 HTTP/1.1\r\n"),
        "request line: {request}"
    );
}

#[tokio::test]
async fn log_defaults_to_dash_container_and_no_query() {
    let (port, captured) = serve_once(http_response("200 OK", "text/plain", "")).await;
    let store = Arc::new(EmbeddedStorage::new());
    seeded_pod_on_node(store.clone(), node(Some("127.0.0.1"), Some(port))).await;
    let (code, text, _) = send(
        store,
        "GET",
        "/api/v1/namespaces/default/pods/web/log",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(text, "");
    let request = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(
        request.starts_with("GET /pods/uid-1/containers/-/log HTTP/1.1\r\n"),
        "{request}"
    );
}

#[tokio::test]
async fn exec_relays_command_and_passes_result_through() {
    let payload = json!({"exitCode": 2, "output": "partial failure"});
    let (port, captured) = serve_once(http_response(
        "200 OK",
        "application/json",
        &payload.to_string(),
    ))
    .await;
    let store = Arc::new(EmbeddedStorage::new());
    seeded_pod_on_node(store.clone(), node(Some("127.0.0.1"), Some(port))).await;

    let (code, status, ctype) = do_request(
        store,
        "POST",
        "/api/v1/namespaces/default/pods/web/exec?container=sidecar",
        Some(json!({"command": ["ls", "-l"]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    assert_eq!(status, payload); // verbatim pass-through incl. exitCode != 0

    let request = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
    assert!(
        request.starts_with("POST /pods/uid-1/containers/sidecar/exec HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request.contains("Content-Type: application/json"),
        "{request}"
    );
    assert!(
        request.ends_with("{\"command\":[\"ls\",\"-l\"]}"),
        "{request}"
    );
}

#[tokio::test]
async fn kubelet_connection_refused_maps_to_502_relay_failure() {
    // Occupy a port, then release it: connecting there is refused reliably.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = listener.local_addr().unwrap().port();
    drop(listener);
    let store = Arc::new(EmbeddedStorage::new());
    seeded_pod_on_node(store.clone(), node(Some("127.0.0.1"), Some(dead_port))).await;
    let (code, status, ..) = do_request(
        store,
        "GET",
        "/api/v1/namespaces/default/pods/web/log",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert!(
        status["message"]
            .as_str()
            .unwrap()
            .starts_with("relay to kubelet failed:"),
        "{status}"
    );
    assert_eq!(status["code"], 502);
}
