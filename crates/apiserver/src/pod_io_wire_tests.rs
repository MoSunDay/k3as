//! Happy-path relay wire tests for T4.2 Scope C.
//!
//! Covers the four relay success/surface behaviors behind Q31 and golden
//! G30: log bytes relayed verbatim onto the kubelet path, default `-`
//! container with no query string, exec command pass-through, and a
//! refused kubelet connection mapping to 502. The shared harness lives in
//! [`crate::pod_io_wire_common`].

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;
use storage::EmbeddedStorage;

use crate::pod_io_wire_common::{
    do_request, http_response, node, seeded_pod_on_node, send, serve_once,
};

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
