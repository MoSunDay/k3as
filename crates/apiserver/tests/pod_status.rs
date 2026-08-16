//! Pod status subresource wire tests (TODO **T4.2**).
//!
//! In-process axum transport (binding.rs pattern) over a shared
//! `EmbeddedStorage`: 200 + `.status` write-through with `resourceVersion`
//! projection (spec preserved), overwrite on a second PUT, 404 on a missing
//! pod, 422 on a body without `.status`, full-pod bodies accepted.

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

fn pod(name: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {"containers": [{"name": "c", "image": "img"}]}
    })
}

#[tokio::test]
async fn status_put_missing_pod_is_404() {
    let store = Arc::new(EmbeddedStorage::new());
    let (code, body) = put(
        "/api/v1/namespaces/default/pods/ghost/status",
        json!({"status": {"phase": "Running"}}),
        store,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "NotFound");
}

#[tokio::test]
async fn status_put_writes_status_and_projects_resource_version() {
    let store = Arc::new(EmbeddedStorage::new());
    let (code, _) = post("/api/v1/namespaces/default/pods", pod("p1"), store.clone()).await;
    assert_eq!(code, StatusCode::CREATED);

    let (code, body) = put(
        "/api/v1/namespaces/default/pods/p1/status",
        json!({"status": {"phase": "Running"}}),
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"]["phase"], "Running");
    // spec + metadata identity preserved from the stored pod.
    assert_eq!(body["metadata"]["name"], "p1");
    assert_eq!(
        body["spec"]["containers"],
        json!([{"name": "c", "image": "img"}])
    );

    // resourceVersion is projected from the write's mod_revision.
    let stored = store
        .get(&storage::Key::new("", "pods", "default", "p1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        body["metadata"]["resourceVersion"],
        json!(stored.mod_revision.to_string())
    );
    assert_eq!(stored.value["status"]["phase"], "Running");
}

#[tokio::test]
async fn status_put_overwrites_existing_status() {
    let store = Arc::new(EmbeddedStorage::new());
    post("/api/v1/namespaces/default/pods", pod("p2"), store.clone()).await;
    let (code, first) = put(
        "/api/v1/namespaces/default/pods/p2/status",
        json!({"status": {"phase": "Running"}}),
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    let (code, second) = put(
        "/api/v1/namespaces/default/pods/p2/status",
        json!({"status": {
            "phase": "Succeeded",
            "conditions": [{"type": "Ready", "status": "False"}]
        }}),
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(second["status"]["phase"], "Succeeded");
    assert_eq!(second["status"]["conditions"][0]["type"], "Ready");
    // Each write bumps the projected resourceVersion.
    assert_ne!(
        first["metadata"]["resourceVersion"],
        second["metadata"]["resourceVersion"]
    );
}

#[tokio::test]
async fn status_put_without_status_field_is_422() {
    let store = Arc::new(EmbeddedStorage::new());
    post("/api/v1/namespaces/default/pods", pod("p3"), store.clone()).await;
    let (code, body) = put(
        "/api/v1/namespaces/default/pods/p3/status",
        json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p3"}}),
        store,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["reason"], "Invalid");
}

#[tokio::test]
async fn status_put_accepts_full_pod_body() {
    let store = Arc::new(EmbeddedStorage::new());
    post("/api/v1/namespaces/default/pods", pod("p4"), store.clone()).await;

    // kubelet PUTs the full pod; only `.status` may change through this route.
    let mut full = pod("p4");
    full["spec"]["containers"][0]["image"] = json!("hijacked");
    full["status"] = json!({"phase": "Running", "podIP": "10.42.0.5"});
    let (code, body) = put(
        "/api/v1/namespaces/default/pods/p4/status",
        full,
        store.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"]["phase"], "Running");
    assert_eq!(body["status"]["podIP"], "10.42.0.5");

    // Body spec/metadata are ignored: the stored spec survives untouched.
    let stored = store
        .get(&storage::Key::new("", "pods", "default", "p4"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.value["spec"]["containers"][0]["image"], "img");
    assert_eq!(stored.value["status"]["podIP"], "10.42.0.5");
}

#[tokio::test]
async fn status_put_succeeds_after_stale_embedded_rv() {
    let store = Arc::new(EmbeddedStorage::new());
    let (code, created) = post("/api/v1/namespaces/default/pods", pod("p5"), store.clone()).await;
    assert_eq!(code, StatusCode::CREATED);
    let projected_rv = created["metadata"]["resourceVersion"].clone();
    assert!(projected_rv.is_string());

    // Plain replace stores the body verbatim: the embedded resourceVersion
    // (the stale projected RV) now lags the entry's mod_revision — exactly
    // the divergence a scheduler bind read-modify-write leaves behind.
    let mut replace = pod("p5");
    replace["metadata"]["resourceVersion"] = projected_rv;
    let (code, _) = put("/api/v1/namespaces/default/pods/p5", replace, store.clone()).await;
    assert_eq!(code, StatusCode::OK);

    // kubelet PUTs the full pod to /status. Before the mod_revision CAS fix
    // this 409'd forever: the handler CASed on the stale embedded RV.
    let mut full = pod("p5");
    full["status"] = json!({"phase": "Running"});
    let (code, body) = put("/api/v1/namespaces/default/pods/p5/status", full, store).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"]["phase"], "Running");
}
