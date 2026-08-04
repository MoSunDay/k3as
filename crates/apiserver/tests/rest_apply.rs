//! T1.2c — Server-Side Apply HTTP integration tests.
//!
//! Exercises the axum REST face with `application/apply-patch+yaml` content
//! type, covering: create-on-absent (201), update (200), conflict (409),
//! force override, and managedFields in the response body.

use std::sync::Arc;

use api::SchemaRegistry;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, StorageBackend};
use tower::ServiceExt;

fn served() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    api::initpro::register(&mut reg);
    reg
}

fn app() -> axum::Router {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    apiserver::api_app(Arc::new(served()), store, "127.0.0.1:6443".to_string())
}

async fn send(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<(&str, Vec<u8>)>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    if let Some((ct, _)) = body {
        b = b.header("content-type", ct);
    }
    let req = match body {
        Some((_, bytes)) => b.body(Body::from(bytes)),
        None => b.body(Body::empty()),
    }
    .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let val = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, val)
}

fn apply_body(name: &str, data: Value) -> Value {
    json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": name, "namespace": "default"},
        "data": data
    })
}

async fn apply_req(
    router: axum::Router,
    name: &str,
    body: &Value,
    field_manager: &str,
    force: bool,
) -> (StatusCode, Value) {
    let mut uri = format!(
        "/api/v1/namespaces/default/configmaps/{}?fieldManager={}",
        name, field_manager
    );
    if force {
        uri.push_str("&force=true");
    }
    send(
        router,
        "PATCH",
        &uri,
        Some((
            "application/apply-patch+yaml",
            serde_json::to_vec(body).unwrap(),
        )),
    )
    .await
}

#[tokio::test]
async fn apply_creates_absent_object_201() {
    let body = apply_body("ssa-cm", json!({"k": "v"}));
    let (status, resp) = apply_req(app(), "ssa-cm", &body, "mgr-a", false).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(resp["data"]["k"], "v");
    assert!(resp["metadata"]["managedFields"].is_array());
    let mf = &resp["metadata"]["managedFields"][0];
    assert_eq!(mf["manager"], "mgr-a");
    assert_eq!(mf["operation"], "Apply");
    assert_eq!(mf["fieldsV1"]["f:data"]["f:k"], json!({}));
}

#[tokio::test]
async fn apply_updates_existing_200() {
    let r = app();
    let body1 = apply_body("ssa-upd", json!({"a": "1"}));
    let (s1, _) = apply_req(r.clone(), "ssa-upd", &body1, "mgr-a", false).await;
    assert_eq!(s1, StatusCode::CREATED);

    let body2 = apply_body("ssa-upd", json!({"a": "1", "b": "2"}));
    let (s2, resp) = apply_req(r, "ssa-upd", &body2, "mgr-a", false).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(resp["data"]["a"], "1");
    assert_eq!(resp["data"]["b"], "2");
}

#[tokio::test]
async fn apply_conflict_returns_409() {
    let r = app();
    let body_a = apply_body("ssa-conf", json!({"key": "a"}));
    let (s1, _) = apply_req(r.clone(), "ssa-conf", &body_a, "mgr-a", false).await;
    assert_eq!(s1, StatusCode::CREATED);

    let body_b = apply_body("ssa-conf", json!({"key": "b"}));
    let (s2, resp) = apply_req(r, "ssa-conf", &body_b, "mgr-b", false).await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(resp["reason"], "Conflict");
    assert!(resp["message"]
        .as_str()
        .unwrap_or("")
        .contains("conflict"));
}

#[tokio::test]
async fn apply_force_overrides_conflict() {
    let r = app();
    let body_a = apply_body("ssa-force", json!({"key": "a"}));
    apply_req(r.clone(), "ssa-force", &body_a, "mgr-a", false).await;

    let body_b = apply_body("ssa-force", json!({"key": "b"}));
    let (s, resp) = apply_req(r, "ssa-force", &body_b, "mgr-b", true).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(resp["data"]["key"], "b");
}

#[tokio::test]
async fn apply_response_has_managed_fields() {
    let body = apply_body("ssa-mf", json!({"k": "v"}));
    let (_, resp) = apply_req(app(), "ssa-mf", &body, "test-mgr", false).await;

    let mf = resp["metadata"]["managedFields"]
        .as_array()
        .expect("managedFields array");
    assert_eq!(mf.len(), 1);
    assert_eq!(mf[0]["manager"], "test-mgr");
    assert_eq!(mf[0]["operation"], "Apply");
    assert_eq!(mf[0]["fieldsType"], "FieldsV1");
}

#[tokio::test]
async fn apply_via_put_works() {
    let body = apply_body("ssa-put", json!({"k": "v"}));
    let uri =
        "/api/v1/namespaces/default/configmaps/ssa-put?fieldManager=put-mgr";
    let (s, resp) = send(
        app(),
        "PUT",
        uri,
        Some((
            "application/apply-patch+yaml",
            serde_json::to_vec(&body).unwrap(),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(
        resp["metadata"]["managedFields"][0]["manager"],
        "put-mgr"
    );
}

#[tokio::test]
async fn apply_prunes_owned_field() {
    let r = app();
    let body1 = apply_body("ssa-prune", json!({"keep": "1", "drop": "2"}));
    apply_req(r.clone(), "ssa-prune", &body1, "mgr-a", false).await;

    let body2 = apply_body("ssa-prune", json!({"keep": "1"}));
    let (s, resp) = apply_req(r, "ssa-prune", &body2, "mgr-a", false).await;
    assert_eq!(s, StatusCode::OK);
    assert!(resp["data"].get("drop").is_none());
    assert_eq!(resp["data"]["keep"], "1");
}
