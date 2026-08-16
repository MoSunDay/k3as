//! Sprint 18 / S3 — wire-level tests for Service defaulting + NodePort
//! allocation at create time (no network, no real etcd — ADR Q17). Mirrors
//! `rest_crud.rs`: the real axum handlers via axum's in-process `oneshot`
//! transport.
//!
//! Decision **D**: NodePort-only dataplane in v1 — ClusterIP Services are
//! creatable/stored but non-forwarding; no `spec.clusterIP` is assigned.

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

/// One router over a fresh embedded store (state is Arc-shared, so cloning
/// the router keeps the same store across requests).
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

/// Minimal Service in `default`; `spec` carries type/ports only.
fn svc(name: &str, spec: Value) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": name, "namespace": "default" },
        "spec": spec,
    })
}

fn message_contains(v: &Value, needle: &str) -> bool {
    v.as_str().unwrap_or_default().contains(needle)
}

fn node_port(body: &Value) -> Option<u64> {
    body.pointer("/spec/ports/0/nodePort")
        .and_then(Value::as_u64)
}

const URI: &str = "/api/v1/namespaces/default/services";

#[tokio::test]
async fn nodeport_allocated_on_create() {
    let r = app();
    let body = svc(
        "np",
        json!({ "type": "NodePort", "ports": [{ "port": 80 }] }),
    );
    let (st, out) = send(r, "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED);
    let np = node_port(&out).expect("nodePort allocated");
    assert!((30000..=32767).contains(&np), "np {np} in range");
}

#[tokio::test]
async fn nodeport_allocation_distinct_and_sequential() {
    let r = app();
    let mut got = Vec::new();
    for i in 0..3 {
        let body = svc(
            &format!("np{i}"),
            json!({ "type": "NodePort", "ports": [{ "port": 80 }] }),
        );
        let (st, out) = send(r.clone(), "POST", URI, json_body(&body)).await;
        assert_eq!(st, StatusCode::CREATED);
        got.push(node_port(&out).expect("nodePort allocated"));
    }
    assert_eq!(got, vec![30000, 30001, 30002], "lowest-free sequential");
}

#[tokio::test]
async fn explicit_nodeport_honored() {
    let r = app();
    let body = svc(
        "np",
        json!({ "type": "NodePort", "ports": [{ "port": 80, "nodePort": 30080 }] }),
    );
    let (st, out) = send(r, "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(node_port(&out), Some(30080));
}

#[tokio::test]
async fn explicit_nodeport_out_of_range_rejected() {
    for bad in [29999u64, 32768] {
        let r = app();
        let body = svc(
            "np",
            json!({ "type": "NodePort", "ports": [{ "port": 80, "nodePort": bad }] }),
        );
        let (st, out) = send(r, "POST", URI, json_body(&body)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "nodePort {bad}");
        assert_eq!(out["reason"], "Invalid");
        assert!(
            message_contains(&out["message"], "nodePort"),
            "message mentions nodePort: {out}"
        );
    }
}

#[tokio::test]
async fn explicit_nodeport_conflict_rejected() {
    let r = app();
    let first = svc(
        "np1",
        json!({ "type": "NodePort", "ports": [{ "port": 80, "nodePort": 30090 }] }),
    );
    let (st, _) = send(r.clone(), "POST", URI, json_body(&first)).await;
    assert_eq!(st, StatusCode::CREATED);
    let second = svc(
        "np2",
        json!({ "type": "NodePort", "ports": [{ "port": 81, "nodePort": 30090 }] }),
    );
    let (st, out) = send(r, "POST", URI, json_body(&second)).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("30090"),
        "message names the taken port: {out}"
    );
}

#[tokio::test]
async fn service_type_defaults_to_clusterip() {
    let r = app();
    let body = svc("c", json!({ "ports": [{ "port": 80 }] }));
    let (st, out) = send(r, "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(out["spec"]["type"], "ClusterIP");
    assert!(node_port(&out).is_none(), "ClusterIP gets no nodePort");
    assert!(out["spec"].get("clusterIP").is_none(), "no clusterIP (D)");
}

#[tokio::test]
async fn loadbalancer_type_also_gets_nodeport() {
    let r = app();
    let body = svc(
        "lb",
        json!({ "type": "LoadBalancer", "ports": [{ "port": 80 }] }),
    );
    let (st, out) = send(r, "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(out["spec"]["type"], "LoadBalancer");
    let np = node_port(&out).expect("nodePort allocated");
    assert!((30000..=32767).contains(&np), "np {np} in range");
}
