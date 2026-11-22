//! Sprint 18 / S3 — wire-level tests for Service defaulting + NodePort
//! allocation at create time (no network, no real etcd — ADR Q17). Mirrors
//! `rest_crud.rs`: the real axum handlers via axum's in-process `oneshot`
//! transport.
//!
//! **Q44** (T4.3 Scope C, Sprint 51) supersedes Decision D: ClusterIPs ARE
//! allocated from `--service-cidr` (default `10.43.0.0/16`) for every
//! non-headless, non-ExternalName Service; the Router carries the VIP
//! traffic (its ClusterIP plane).

use std::net::Ipv4Addr;
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

/// True when the string is an IPv4 inside the default service CIDR
/// `10.43.0.0/16` (Q44: the test-facing app allocates from that pool).
fn in_default_cidr(s: &str) -> bool {
    match s.parse::<Ipv4Addr>() {
        Ok(ip) => {
            let n = u32::from(ip);
            (n >> 16) == ((10 << 8) | 43)
        }
        Err(_) => false,
    }
}

fn cluster_ip(body: &Value) -> Option<&str> {
    body.pointer("/spec/clusterIP").and_then(Value::as_str)
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
    // Q44 (T4.3 Scope C): a clusterIP IS allocated, inside the default CIDR.
    let ip = cluster_ip(&out).expect("clusterIP allocated (Q44)");
    assert!(in_default_cidr(ip), "clusterIP {ip} inside 10.43.0.0/16");
}

#[tokio::test]
async fn clusterip_allocated_distinct_and_sequential() {
    let r = app();
    let mut got = Vec::new();
    for i in 0..3 {
        let body = svc(&format!("cip{i}"), json!({ "ports": [{ "port": 80 }] }));
        let (st, out) = send(r.clone(), "POST", URI, json_body(&body)).await;
        assert_eq!(st, StatusCode::CREATED);
        let ip = cluster_ip(&out).expect("clusterIP allocated");
        assert!(in_default_cidr(ip), "clusterIP {ip} inside 10.43.0.0/16");
        got.push(ip.to_string());
    }
    assert_eq!(
        got,
        vec!["10.43.0.1", "10.43.0.2", "10.43.0.3"],
        "lowest-free sequential"
    );
}

#[tokio::test]
async fn explicit_in_cidr_clusterip_honored_verbatim() {
    let r = app();
    let body = svc(
        "cip",
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.9.9" }),
    );
    let (st, out) = send(r, "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(cluster_ip(&out), Some("10.43.9.9"), "explicit verbatim");
}

#[tokio::test]
async fn explicit_out_of_cidr_clusterip_rejected() {
    for bad in ["10.44.0.5", "192.168.1.1"] {
        let r = app();
        let body = svc(
            "cip",
            json!({ "ports": [{ "port": 80 }], "clusterIP": bad }),
        );
        let (st, out) = send(r, "POST", URI, json_body(&body)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "clusterIP {bad}");
        assert_eq!(out["reason"], "Invalid");
        assert!(
            message_contains(&out["message"], "10.43.0.0/16"),
            "message names the service CIDR: {out}"
        );
        assert!(
            message_contains(&out["message"], bad),
            "message names the IP: {out}"
        );
    }
}

#[tokio::test]
async fn duplicate_explicit_clusterip_rejected() {
    let r = app();
    let first = svc(
        "c1",
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.4.4" }),
    );
    let (st, _) = send(r.clone(), "POST", URI, json_body(&first)).await;
    assert_eq!(st, StatusCode::CREATED);
    let second = svc(
        "c2",
        json!({ "ports": [{ "port": 81 }], "clusterIP": "10.43.4.4" }),
    );
    let (st, out) = send(r, "POST", URI, json_body(&second)).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        message_contains(&out["message"], "already allocated"),
        "message names the collision: {out}"
    );
}

#[tokio::test]
async fn headless_clusterip_none_preserved_verbatim() {
    let r = app();
    let body = svc(
        "headless",
        json!({ "ports": [{ "port": 80 }], "clusterIP": "None" }),
    );
    let (st, out) = send(r, "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(
        cluster_ip(&out),
        Some("None"),
        "headless stays verbatim, no allocation"
    );
}

#[tokio::test]
async fn externalname_gets_no_clusterip() {
    let r = app();
    let body = svc("en", json!({ "externalName": "db.example.com" }));
    let (st, out) = send(r, "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED);
    assert!(
        out.pointer("/spec/clusterIP").is_none(),
        "ExternalName untouched: {out}"
    );
}

#[tokio::test]
async fn nodeport_service_also_gets_clusterip() {
    // Q44: every non-headless Service has a VIP (NodePort included) —
    // upstream parity with k8s.
    let r = app();
    let body = svc(
        "np",
        json!({ "type": "NodePort", "ports": [{ "port": 80 }] }),
    );
    let (st, out) = send(r, "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED);
    assert!(node_port(&out).is_some(), "nodePort still allocated");
    let ip = cluster_ip(&out).expect("clusterIP allocated (Q44)");
    assert!(in_default_cidr(ip), "clusterIP {ip} inside 10.43.0.0/16");
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
