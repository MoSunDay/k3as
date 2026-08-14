//! T3.1b wire-level finalizer semantics (decision **Q20**): finalizer-gated
//! soft DELETE (deletionTimestamp), idempotent repeat DELETE, finalizer
//! completion via PUT/PATCH, namespace `kubernetes` finalizer injection, and
//! `propagationPolicy=Orphan`. In-process axum transport (rest_crud.rs
//! pattern); JSON-only wire (Q10).

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

fn json_body(v: &Value) -> Option<(&'static str, Vec<u8>)> {
    Some(("application/json", serde_json::to_vec(v).unwrap()))
}

fn configmap(name: &str, finalizers: Value) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": name, "namespace": "default", "finalizers": finalizers},
        "data": {"k": "v"},
    })
}

const CM_PATH: &str = "/api/v1/namespaces/default/configmaps/fin-cm";

#[tokio::test]
async fn delete_with_finalizers_soft_deletes_and_stamps_timestamp() {
    let r = app();
    let (st, _) = send(
        r.clone(),
        "POST",
        "/api/v1/namespaces/default/configmaps",
        json_body(&configmap("fin-cm", json!(["x"]))),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (st, body) = send(r.clone(), "DELETE", CM_PATH, None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let ts = body["metadata"]["deletionTimestamp"]
        .as_str()
        .expect("deletionTimestamp stamped")
        .to_string();
    assert!(ts.ends_with('Z') && ts.len() == 20, "RFC3339: {ts}");
    // Finalizer still present: the object must survive.
    assert_eq!(
        body["metadata"]["finalizers"],
        json!(["x"]),
        "finalizers preserved on soft delete"
    );

    // GET still 200: soft-deleted objects remain readable.
    let (st, body) = send(r.clone(), "GET", CM_PATH, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["metadata"]["deletionTimestamp"], json!(ts));

    // Repeat DELETE is idempotent: 200, timestamp unchanged.
    let (st, body2) = send(r.clone(), "DELETE", CM_PATH, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body2["metadata"]["deletionTimestamp"],
        json!(ts),
        "second DELETE keeps the original timestamp"
    );
}

#[tokio::test]
async fn put_emptying_finalizers_completes_the_delete() {
    let r = app();
    send(
        r.clone(),
        "POST",
        "/api/v1/namespaces/default/configmaps",
        json_body(&configmap("fin-cm", json!(["x"]))),
    )
    .await;
    let (st, _) = send(r.clone(), "DELETE", CM_PATH, None).await;
    assert_eq!(st, StatusCode::OK);

    // PUT with the finalizer removed (the body carries the timestamp read
    // from GET, as any real client would): stored body has deletionTimestamp
    // + empty finalizers -> terminal delete (finalizer-completion rule).
    let mut live = body_of(&r, CM_PATH).await;
    live["metadata"]["finalizers"] = json!([]);
    let (st, body) = send(r.clone(), "PUT", CM_PATH, json_body(&live)).await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let (st, _) = send(r.clone(), "GET", CM_PATH, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "object removed on completion");
}

/// Read the live object via GET (CAS precondition + current state).
async fn body_of(r: &axum::Router, path: &str) -> Value {
    let (_, body) = send(r.clone(), "GET", path, None).await;
    body
}

#[tokio::test]
async fn patch_emptying_finalizers_completes_the_delete() {
    let r = app();
    send(
        r.clone(),
        "POST",
        "/api/v1/namespaces/default/configmaps",
        json_body(&configmap("fin-cm", json!(["x"]))),
    )
    .await;
    send(r.clone(), "DELETE", CM_PATH, None).await;

    let patch = json!({"metadata": {"finalizers": [],
        "resourceVersion": body_of(&r, CM_PATH).await["metadata"]["resourceVersion"].clone()}});
    let (st, body) = send(r.clone(), "PATCH", CM_PATH, json_body(&patch)).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, _) = send(r.clone(), "GET", CM_PATH, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn namespace_create_injects_kubernetes_finalizer() {
    let r = app();
    let ns = json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {"name": "ns-inject"},
    });
    let (st, body) = send(r.clone(), "POST", "/api/v1/namespaces", json_body(&ns)).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["metadata"]["finalizers"], json!(["kubernetes"]));

    // DELETE on the finalizer-carrying Namespace soft-deletes (timestamp set,
    // object retained) -- the namespace controller owns the drain.
    let (st, body) = send(r.clone(), "DELETE", "/api/v1/namespaces/ns-inject", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body["metadata"]["deletionTimestamp"].is_string());
    let (st, _) = send(r.clone(), "GET", "/api/v1/namespaces/ns-inject", None).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn orphan_propagation_annotates_then_hard_deletes() {
    let r = app();
    send(
        r.clone(),
        "POST",
        "/api/v1/namespaces/default/configmaps",
        json_body(&configmap("fin-cm", json!([]))),
    )
    .await;

    let (st, body) = send(
        r.clone(),
        "DELETE",
        "/api/v1/namespaces/default/configmaps/fin-cm?propagationPolicy=Orphan",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // The DELETE response (and thus the event the GC consumes) carries the
    // propagation marker.
    assert_eq!(
        body["metadata"]["annotations"]["init-pro.io/deletion-propagation"],
        json!("Orphan")
    );
    let (st, _) = send(r.clone(), "GET", CM_PATH, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "orphan delete is a hard delete");
}

#[tokio::test]
async fn plain_delete_of_finalizerless_object_is_unchanged() {
    // Golden G10/G11 shape: no finalizers, no propagation policy -> hard
    // delete, 200 + removed object, then 404.
    let r = app();
    send(
        r.clone(),
        "POST",
        "/api/v1/namespaces/default/configmaps",
        json_body(&configmap("fin-cm", json!([]))),
    )
    .await;
    let (st, body) = send(r.clone(), "DELETE", CM_PATH, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["kind"], "ConfigMap");
    assert!(body["metadata"]["deletionTimestamp"].is_null());
    let (st, _) = send(r.clone(), "GET", CM_PATH, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
