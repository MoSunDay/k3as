//! T1.2b wire-level CRUD round-trip against the embedded store (no network,
//! no real etcd — ADR Q17). Mirrors `discovery_http.rs`: the real axum handlers
//! via axum's in-process `oneshot` transport.
//!
//! Covers: create/get/list/update(CAS)/delete/patch + scope rules + grouped
//! (init-pro.io) CRUD. Server-side apply field-manager is deferred to T1.2c.

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

/// One router over a fresh embedded store (state is Arc-shared, so cloning the
/// router keeps the same store across requests).
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
    let mut b = Request::builder().method(Method::from_bytes(method.as_bytes()).unwrap()).uri(uri);
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
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let val = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, val)
}

fn json_body(v: &Value) -> Option<(&'static str, Vec<u8>)> {
    Some(("application/json", serde_json::to_vec(v).unwrap()))
}

fn cm(name: &str, data: Value, rv: Option<&str>) -> Value {
    let mut meta = json!({ "name": name, "namespace": "default" });
    if let Some(r) = rv {
        meta["resourceVersion"] = json!(r);
    }
    json!({ "apiVersion": "v1", "kind": "ConfigMap", "metadata": meta, "data": data })
}

#[tokio::test]
async fn create_get_roundtrip() {
    let r = app();
    let (st, body) =
        send(r.clone(), "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm("cm1", json!({"k": "v"}), None))).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(body["kind"], "ConfigMap");
    assert_eq!(body["metadata"]["namespace"], "default");
    let rv = body["metadata"]["resourceVersion"].as_str().expect("resourceVersion set");
    assert!(!rv.is_empty());
    assert_eq!(body["data"]["k"], "v");

    let (st, body) = send(r, "GET", "/api/v1/namespaces/default/configmaps/cm1", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["metadata"]["resourceVersion"], rv);
    assert_eq!(body["data"]["k"], "v");
}

#[tokio::test]
async fn create_already_exists_is_409() {
    let r = app();
    let (st, _) =
        send(r.clone(), "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm("dup", json!({}), None))).await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, body) = send(r, "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm("dup", json!({}), None))).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "AlreadyExists");
}

#[tokio::test]
async fn get_not_found_is_404() {
    let r = app();
    let (st, body) = send(r, "GET", "/api/v1/namespaces/default/configmaps/nope", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "NotFound");
}

#[tokio::test]
async fn list_empty_then_populated() {
    let r = app();
    let (st, body) = send(r.clone(), "GET", "/api/v1/namespaces/default/configmaps", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["kind"], "ConfigMapList");
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    assert!(body["metadata"]["resourceVersion"].as_str().is_some());

    send(r.clone(), "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm("a", json!({}), None))).await;
    send(r.clone(), "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm("b", json!({}), None))).await;
    let (st, body) = send(r, "GET", "/api/v1/namespaces/default/configmaps", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn update_cas_conflict_on_stale_resourceversion() {
    let r = app();
    let (_, body) =
        send(r.clone(), "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm("c", json!({"a": "1"}), None))).await;
    let rv = body["metadata"]["resourceVersion"].as_str().unwrap().to_string();

    // correct rv -> 200, new rv assigned
    let (st, body) =
        send(r.clone(), "PUT", "/api/v1/namespaces/default/configmaps/c", json_body(&cm("c", json!({"a": "2"}), Some(&rv)))).await;
    assert_eq!(st, StatusCode::OK);
    let rv2 = body["metadata"]["resourceVersion"].as_str().unwrap();
    assert_ne!(rv2, rv);

    // stale rv (the old one) -> 409 Conflict
    let (st, body) =
        send(r.clone(), "PUT", "/api/v1/namespaces/default/configmaps/c", json_body(&cm("c", json!({"a": "3"}), Some(&rv)))).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "Conflict");
}

#[tokio::test]
async fn delete_then_get_404() {
    let r = app();
    send(r.clone(), "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm("d", json!({}), None))).await;
    let (st, body) = send(r.clone(), "DELETE", "/api/v1/namespaces/default/configmaps/d", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["kind"], "ConfigMap");
    let (st, _) = send(r, "GET", "/api/v1/namespaces/default/configmaps/d", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn patch_strategic_merge_deep_merges_data() {
    let r = app();
    send(r.clone(), "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm("p", json!({"a": "1"}), None))).await;

    let patch = json!({ "data": { "b": "2" } });
    let (st, body) = send(
        r.clone(),
        "PATCH",
        "/api/v1/namespaces/default/configmaps/p",
        Some(("application/strategic-merge-patch+json", serde_json::to_vec(&patch).unwrap())),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["data"]["a"], "1"); // preserved
    assert_eq!(body["data"]["b"], "2"); // merged in
}

#[tokio::test]
async fn patch_merge_patch_replaces_data() {
    let r = app();
    send(r.clone(), "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm("m", json!({"a": "1"}), None))).await;

    // RFC 7386 merge-patch on a scalar map merges too (data is a map of scalars).
    let patch = json!({ "data": { "b": "2" } });
    let (st, body) = send(
        r,
        "PATCH",
        "/api/v1/namespaces/default/configmaps/m",
        Some(("application/merge-patch+json", serde_json::to_vec(&patch).unwrap())),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["data"]["a"], "1");
    assert_eq!(body["data"]["b"], "2");
}

#[tokio::test]
async fn patch_json_patch_adds_field() {
    let r = app();
    send(r.clone(), "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm("j", json!({"a": "1"}), None))).await;

    let patch = json!([ { "op": "add", "path": "/data/b", "value": "2" } ]);
    let (st, body) = send(
        r,
        "PATCH",
        "/api/v1/namespaces/default/configmaps/j",
        Some(("application/json-patch+json", serde_json::to_vec(&patch).unwrap())),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["data"]["a"], "1");
    assert_eq!(body["data"]["b"], "2");
}

#[tokio::test]
async fn cluster_scoped_namespace_crud() {
    let r = app();
    let ns = json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "ns1" } });
    let (st, body) = send(r.clone(), "POST", "/api/v1/namespaces", json_body(&ns)).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(body["kind"], "Namespace");

    let (st, body) = send(r, "GET", "/api/v1/namespaces/ns1", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["metadata"]["name"], "ns1");
}

#[tokio::test]
async fn scope_mismatch_yields_404() {
    let r = app();
    // namespaced resource via the cluster (no-namespace) create path -> 404
    let (st, _) = send(r.clone(), "POST", "/api/v1/configmaps", json_body(&cm("x", json!({}), None))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // cluster-scoped resource via the namespaced list path -> 404
    let (st, _) = send(r, "GET", "/api/v1/namespaces/default/nodes", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_resource_is_404() {
    let r = app();
    let (st, body) = send(r, "GET", "/api/v1/fabricated", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "NotFound");
}

#[tokio::test]
async fn grouped_crd_crud() {
    let r = app();
    let lr = json!({ "apiVersion": "init-pro.io/v1", "kind": "LuaRouter", "metadata": { "name": "lr1", "namespace": "default" }, "spec": { "routes": [] } });
    let (st, body) = send(r.clone(), "POST", "/apis/init-pro.io/v1/namespaces/default/luarouters", json_body(&lr)).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(body["kind"], "LuaRouter");

    let (st, body) = send(r.clone(), "GET", "/apis/init-pro.io/v1/namespaces/default/luarouters", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["kind"], "LuaRouterList");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    let (st, _) = send(r, "GET", "/apis/init-pro.io/v1/namespaces/default/luarouters/lr1", None).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn list_pagination_key_cursor() {
    let r = app();
    for i in 0..5 {
        send(r.clone(), "POST", "/api/v1/namespaces/default/configmaps", json_body(&cm(&format!("c{i}"), json!({}), None))).await;
    }
    // page size 2 -> 2 items + a continue token
    let (st, body) = send(r.clone(), "GET", "/api/v1/namespaces/default/configmaps?limit=2", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    let cont = body["metadata"]["continue"].as_str().unwrap();
    assert!(!cont.is_empty(), "continue token must be set when more pages remain");

    // follow the cursor -> next page
    let next = format!("/api/v1/namespaces/default/configmaps?limit=2&continue={cont}");
    let (st, body) = send(r, "GET", &next, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
}
