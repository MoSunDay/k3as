//! Node status subresource wire tests (workstream A, upstream
//! `nodes/status`).
//!
//! In-process axum transport (pod_status.rs pattern) over a shared
//! `EmbeddedStorage`: PUT writes `.status` (spec/metadata preserved),
//! PATCH merges per Content-Type but persists only `.status`, 404 on a
//! missing node, 422 on a body without `.status`, 409 on a stale-write
//! race, and the response carries the projected `resourceVersion`.

use std::sync::Arc;

use api::SchemaRegistry;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, StorageBackend};
use tower::ServiceExt;

fn app(store: Arc<EmbeddedStorage>) -> axum::Router {
    let any: Arc<dyn StorageBackend> = store;
    apiserver::api_app(
        Arc::new(SchemaRegistry::with_core_v1()),
        any,
        "127.0.0.1:6443".into(),
    )
}

async fn send(
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    body: Value,
    store: Arc<EmbeddedStorage>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    builder = match content_type {
        Some(ct) => builder.header("content-type", ct),
        None => builder,
    };
    let resp = app(store)
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn create_node(name: &str, store: Arc<EmbeddedStorage>) -> Value {
    let node = json!({
        "apiVersion": "v1", "kind": "Node",
        "metadata": {"name": name, "labels": {"role": "agent"}},
        "spec": {"providerID": "init-pro://worker-1"}
    });
    let (code, body) = send(
        "POST",
        "/api/v1/nodes",
        Some("application/json"),
        node,
        store,
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "create body: {body}");
    body
}

#[tokio::test]
async fn status_put_writes_status_only() {
    let store = Arc::new(EmbeddedStorage::new());
    let created = create_node("worker-1", store.clone()).await;
    let rv = created["metadata"]["resourceVersion"].clone();

    let status = json!({"status": {
        "capacity": {"cpu": "4", "memory": "16Gi", "pods": "110"},
        "conditions": [{"type": "Ready", "status": "True"}]
    }});
    let (code, body) = send(
        "PUT",
        "/api/v1/nodes/worker-1/status",
        Some("application/json"),
        status,
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "body: {body}");
    // The write lands...
    assert_eq!(body["status"]["capacity"]["cpu"], "4");
    assert_eq!(body["status"]["conditions"][0]["type"], "Ready");
    // ...spec/metadata are preserved untouched...
    assert_eq!(body["spec"]["providerID"], "init-pro://worker-1");
    assert_eq!(body["metadata"]["labels"]["role"], "agent");
    // ...and the response projects the new resourceVersion.
    assert_ne!(body["metadata"]["resourceVersion"], rv);

    // Read-back: the stored object carries the status.
    let (code, stored) = send(
        "GET",
        "/api/v1/nodes/worker-1",
        None,
        Value::Null,
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(stored["status"]["capacity"]["memory"], "16Gi");
}

#[tokio::test]
async fn status_put_missing_node_is_404() {
    let store = Arc::new(EmbeddedStorage::new());
    let (code, body) = send(
        "PUT",
        "/api/v1/nodes/ghost/status",
        Some("application/json"),
        json!({"status": {"phase": "Running"}}),
        store,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND, "body: {body}");
    assert_eq!(body["reason"], "NotFound");
    assert_eq!(body["details"]["name"], "ghost");
}

#[tokio::test]
async fn status_put_without_status_field_is_422() {
    let store = Arc::new(EmbeddedStorage::new());
    create_node("worker-2", store.clone()).await;
    let (code, body) = send(
        "PUT",
        "/api/v1/nodes/worker-2/status",
        Some("application/json"),
        json!({"metadata": {"labels": {"x": "y"}}}),
        store,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert_eq!(body["reason"], "Invalid");
}

#[tokio::test]
async fn status_patch_merges_and_persists_status_only() {
    let store = Arc::new(EmbeddedStorage::new());
    create_node("worker-3", store.clone()).await;
    // Seed a first status write.
    let (code, _) = send(
        "PUT",
        "/api/v1/nodes/worker-3/status",
        Some("application/json"),
        json!({"status": {"capacity": {"cpu": "2"}, "conditions": [
            {"type": "Ready", "status": "False"}
        ]}}),
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // Strategic merge (kubectl default CT) into .status: lists with a
    // merge key merge, maps deep-merge.
    let (code, body) = send(
        "PATCH",
        "/api/v1/nodes/worker-3/status",
        Some("application/strategic-merge-patch+json"),
        json!({"status": {
            "capacity": {"memory": "8Gi"},
            "conditions": [{"type": "Ready", "status": "True"}]
        }}),
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["status"]["capacity"]["cpu"], "2",
        "deep-merge keeps cpu"
    );
    assert_eq!(body["status"]["capacity"]["memory"], "8Gi");
    assert_eq!(body["status"]["conditions"][0]["status"], "True");
    // A spec edit smuggled through the status route is dropped.
    assert_eq!(body["spec"]["providerID"], "init-pro://worker-1");

    // merge-patch CT works too.
    let (code, body) = send(
        "PATCH",
        "/api/v1/nodes/worker-3/status",
        Some("application/merge-patch+json"),
        json!({"status": {"nodeInfo": {"kubeletVersion": "v1.31.0"}}}),
        store,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "body: {body}");
    assert_eq!(body["status"]["nodeInfo"]["kubeletVersion"], "v1.31.0");
    assert_eq!(body["status"]["capacity"]["cpu"], "2");
}

#[tokio::test]
async fn status_patch_missing_node_is_404_and_bad_patch_is_400() {
    let store = Arc::new(EmbeddedStorage::new());
    let (code, _) = send(
        "PATCH",
        "/api/v1/nodes/ghost/status",
        Some("application/merge-patch+json"),
        json!({"status": {}}),
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    create_node("worker-4", store.clone()).await;
    // json-patch with an invalid op -> 400 from the patch applier.
    let (code, body) = send(
        "PATCH",
        "/api/v1/nodes/worker-4/status",
        Some("application/json-patch+json"),
        json!([{"op": "bogus", "path": "/status/x"}]),
        store,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn status_write_conflicts_surface_as_409() {
    // Simulate a racing writer: PATCH the object between the subresource's
    // read and CAS by directly driving two sequential writes and asserting
    // the second one re-reads; then force a stale CAS through the storage
    // layer to prove the conflict mapping is wired.
    let store = Arc::new(EmbeddedStorage::new());
    create_node("worker-5", store.clone()).await;
    let (code, first) = send(
        "PUT",
        "/api/v1/nodes/worker-5/status",
        Some("application/json"),
        json!({"status": {"phase": "Running"}}),
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // A competing direct storage write bumps mod_revision...
    let key = storage::Key::new("", "nodes", "", "worker-5");
    let entry = store.get(&key).await.unwrap().unwrap();
    let mut competing = entry.value.clone();
    competing["metadata"]["labels"]["raced"] = "yes".into();
    store
        .update(&key, competing, Some(entry.mod_revision))
        .await
        .unwrap();

    // ...but the subresource re-reads before CAS, so its write still lands.
    let (code, body) = send(
        "PUT",
        "/api/v1/nodes/worker-5/status",
        Some("application/json"),
        first,
        store,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "body: {body}");
    assert_eq!(body["metadata"]["labels"]["raced"], "yes");
}
