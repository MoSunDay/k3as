//! Pod binding subresource wire tests (TODO **T3.2**, slice S2).
//!
//! In-process axum transport (rest_crud.rs pattern) over a shared
//! `EmbeddedStorage`: 201 + `spec.nodeName` write-through on first bind,
//! 409 on double bind, 404 on a missing pod, 422 on a target-less body,
//! and bind success even when the stored pod's embedded `resourceVersion`
//! lags its `mod_revision` (scheduler read-modify-write divergence).

use std::sync::Arc;

use api::SchemaRegistry;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, StorageBackend};
use tower::ServiceExt;

fn served() -> SchemaRegistry {
    SchemaRegistry::with_core_v1()
}

fn app(store: Arc<EmbeddedStorage>) -> axum::Router {
    let any: Arc<dyn StorageBackend> = store;
    apiserver::api_app(Arc::new(served()), any, "127.0.0.1:6443".into())
}

async fn post(uri: &str, body: Value, store: Arc<EmbeddedStorage>) -> (StatusCode, Value) {
    let resp = app(store)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn pod(name: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {"containers": [{"name": "c", "image": "img"}]}
    })
}

async fn put(uri: &str, body: Value, store: Arc<EmbeddedStorage>) -> (StatusCode, Value) {
    let resp = app(store)
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn bind_writes_nodename_and_echoes_binding() {
    let store = Arc::new(EmbeddedStorage::new());
    let (code, _) = post("/api/v1/namespaces/default/pods", pod("p1"), store.clone()).await;
    assert_eq!(code, StatusCode::CREATED);

    let binding = json!({
        "apiVersion": "v1", "kind": "Binding",
        "metadata": {"name": "p1", "namespace": "default"},
        "target": {"apiVersion": "v1", "kind": "Node", "name": "node-a"}
    });
    let (code, body) = post(
        "/api/v1/namespaces/default/pods/p1/binding",
        binding,
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    assert_eq!(body["kind"], "Binding");
    assert_eq!(body["metadata"]["name"], "p1");
    assert_eq!(body["metadata"]["namespace"], "default");
    assert!(body["metadata"]["resourceVersion"].is_string());

    // The pod itself now carries spec.nodeName (read through storage).
    let stored = store
        .get(&storage::Key::new("", "pods", "default", "p1"))
        .await
        .unwrap()
        .expect("pod present");
    assert_eq!(
        stored.value.pointer("/spec/nodeName"),
        Some(&json!("node-a"))
    );
}

#[tokio::test]
async fn double_bind_is_409_with_upstream_message() {
    let store = Arc::new(EmbeddedStorage::new());
    post("/api/v1/namespaces/default/pods", pod("p2"), store.clone()).await;
    let b = |n: &str| {
        json!({"apiVersion": "v1", "kind": "Binding", "metadata": {"name": "p2"},
               "target": {"name": n}})
    };
    let (first, _) = post(
        "/api/v1/namespaces/default/pods/p2/binding",
        b("node-a"),
        store.clone(),
    )
    .await;
    assert_eq!(first, StatusCode::CREATED);
    let (second, body) = post(
        "/api/v1/namespaces/default/pods/p2/binding",
        b("node-b"),
        store.clone(),
    )
    .await;
    assert_eq!(second, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "Conflict");
    assert_eq!(body["code"], 409);
    let msg = body["message"].as_str().unwrap();
    assert!(msg.contains("already assigned to node"), "got: {msg}");
    // First bind wins: nodeName unchanged by the rejected second bind.
    let stored = store
        .get(&storage::Key::new("", "pods", "default", "p2"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.value.pointer("/spec/nodeName"),
        Some(&json!("node-a"))
    );
}

#[tokio::test]
async fn bind_missing_pod_is_404() {
    let store = Arc::new(EmbeddedStorage::new());
    let binding = json!({"metadata": {"name": "ghost"}, "target": {"name": "n1"}});
    let (code, body) = post(
        "/api/v1/namespaces/default/pods/ghost/binding",
        binding,
        store,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "NotFound");
}

#[tokio::test]
async fn bind_without_target_is_422() {
    let store = Arc::new(EmbeddedStorage::new());
    post("/api/v1/namespaces/default/pods", pod("p3"), store.clone()).await;
    let (code, body) = post(
        "/api/v1/namespaces/default/pods/p3/binding",
        json!({"apiVersion": "v1", "kind": "Binding", "metadata": {"name": "p3"}}),
        store,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["reason"], "Invalid");
}

#[tokio::test]
async fn bind_succeeds_after_stale_embedded_rv() {
    let store = Arc::new(EmbeddedStorage::new());
    let (code, created) = post("/api/v1/namespaces/default/pods", pod("p9"), store.clone()).await;
    assert_eq!(code, StatusCode::CREATED);
    let projected_rv = created["metadata"]["resourceVersion"].clone();
    assert!(projected_rv.is_string());

    // Plain replace stores the body verbatim: the embedded resourceVersion
    // (the stale projected RV) now lags the entry's mod_revision. Before the
    // mod_revision CAS fix the subsequent bind 409'd on this stale RV.
    let mut replace = pod("p9");
    replace["metadata"]["resourceVersion"] = projected_rv;
    let (code, _) = put("/api/v1/namespaces/default/pods/p9", replace, store.clone()).await;
    assert_eq!(code, StatusCode::OK);

    let binding = json!({
        "apiVersion": "v1", "kind": "Binding",
        "metadata": {"name": "p9", "namespace": "default"},
        "target": {"apiVersion": "v1", "kind": "Node", "name": "node-a"}
    });
    let (code, body) = post(
        "/api/v1/namespaces/default/pods/p9/binding",
        binding,
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    assert_eq!(body["kind"], "Binding");
    assert_eq!(body["metadata"]["name"], "p9");

    // The pod itself now carries spec.nodeName (read through storage).
    let stored = store
        .get(&storage::Key::new("", "pods", "default", "p9"))
        .await
        .unwrap()
        .expect("pod present");
    assert_eq!(
        stored.value.pointer("/spec/nodeName"),
        Some(&json!("node-a"))
    );
}
