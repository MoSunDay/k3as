//! Pre-hop resolution and validation branch tests for the T4.2 Scope C
//! pod_io relay.
//!
//! Each failure here must short-circuit before any hop to the kubelet
//! (Q31): 404 unknown pod / unscheduled pod / missing node object / pod
//! without uid, 502 node without an InternalIP address, and 400 for an
//! invalid tailLines parameter or exec body. Part of the G30 surface; the
//! shared harness lives in [`crate::pod_io_wire_common`].

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;
use storage::EmbeddedStorage;

use crate::pod_io_wire_common::{do_request, node, pod, seed, seeded_pod_on_node};

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
