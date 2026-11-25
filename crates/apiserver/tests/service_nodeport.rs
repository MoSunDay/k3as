//! Sprint 18 / S3 — wire-level tests for Service defaulting + NodePort
//! allocation at create time (no network, no real etcd — ADR Q17). Mirrors
//! `rest_crud.rs`: the real axum handlers via axum's in-process `oneshot`
//! transport.
//!
//! **Q44** (T4.3 Scope C, Sprint 51) supersedes Decision D: ClusterIPs ARE
//! allocated from `--service-cidr` (default `10.43.0.0/16`) for every
//! non-headless, non-ExternalName Service; the Router carries the VIP
//! traffic (its ClusterIP plane).
//!
//! **S52-D0** (T4.3 Scope C follow-up / Q44): write-path admission — a set
//! clusterIP/nodePort is immutable across PUT / PATCH / SSA apply (change
//! or drop -> 422), otherwise the create-path judgment (validate-or-
//! allocate) runs on the final object.
//!
//! **S53 P1-4** (Q44): `nodePort: 0` = unspecified (create allocates, update retains).

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
    app_seeded_with(store)
}

/// The same router over a caller-seeded store (S52-D0 tests need stored
/// legacy Services that pre-date clusterIP allocation).
fn app_seeded_with(store: Arc<dyn StorageBackend>) -> axum::Router {
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

// S53 P1-4 (Q44): nodePort 0 = unspecified -> 201, allocated port stored.
#[tokio::test]
async fn nodeport_zero_means_unspecified_auto_allocates() {
    let r = app();
    let body = svc(
        "np0",
        json!({ "type": "NodePort", "ports": [{ "port": 80, "nodePort": 0 }] }),
    );
    let (st, out) = send(r.clone(), "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED, "{out}");
    let np = node_port(&out).expect("nodePort allocated");
    assert!((30000..=32767).contains(&np), "np {np} in range");
    let (st, got) = send(r, "GET", &item("np0"), None).await;
    assert_eq!((st, node_port(&got)), (StatusCode::OK, Some(np)));
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

// --- S52-D0 (T4.3 Scope C follow-up / Q44): write-path admission ---------

/// Item URI for one Service in `default`.
fn item(name: &str) -> String {
    format!("{URI}/{name}")
}

/// Create a ClusterIP Service, return its allocated clusterIP.
async fn created_with_ip(r: &axum::Router, name: &str) -> String {
    let body = svc(name, json!({ "ports": [{ "port": 80 }] }));
    let (st, out) = send(r.clone(), "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED);
    cluster_ip(&out).expect("clusterIP allocated").to_string()
}

/// A full PUT body echoing `name` with an explicit clusterIP.
fn put_body(name: &str, cluster_ip: &str) -> Value {
    svc(
        name,
        json!({ "type": "ClusterIP", "ports": [{ "port": 80 }], "clusterIP": cluster_ip }),
    )
}

#[tokio::test]
async fn put_with_stale_rv_conflict_wins_over_admission() {
    // Golden `put-n` parity: a stale-replace CAS conflict (409) must win
    // over the S52-D0 admission (422) even when the superseded body also
    // "changes" the allocated clusterIP.
    let r = app();
    let ip = created_with_ip(&r, "web").await;
    let (st, cur) = send(r.clone(), "GET", &item("web"), None).await;
    assert_eq!(st, StatusCode::OK);
    let stale_rv = cur["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .to_string();
    // Bump the stored revision with a valid replace (clusterIP unchanged).
    let mut ok = put_body("web", &ip);
    ok["metadata"]["resourceVersion"] = json!(stale_rv);
    let (st, _) = send(r.clone(), "PUT", &item("web"), json_body(&ok)).await;
    assert_eq!(st, StatusCode::OK);
    // Stale rv + a clusterIP "change": 409 Conflict, never 422 Invalid.
    let mut stale = put_body("web", "10.43.99.99");
    stale["metadata"]["resourceVersion"] = json!(stale_rv);
    let (st, out) = send(r, "PUT", &item("web"), json_body(&stale)).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(out["reason"], "Conflict");
}

#[tokio::test]
async fn put_changing_set_clusterip_is_422() {
    let r = app();
    let ip = created_with_ip(&r, "web").await;
    let (st, out) = send(
        r.clone(),
        "PUT",
        &item("web"),
        json_body(&put_body("web", "10.43.99.99")),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(out["reason"], "Invalid");
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains(ip.as_str()),
        "message names the stored IP {ip}: {out}"
    );
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("may not change once set"),
        "message states immutability: {out}"
    );
    // The stored object is untouched.
    let (st, got) = send(r, "GET", &item("web"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(cluster_ip(&got), Some(ip.as_str()));
}

#[tokio::test]
async fn put_keeping_clusterip_identical_is_200() {
    let r = app();
    let ip = created_with_ip(&r, "web").await;
    let (st, out) = send(
        r.clone(),
        "PUT",
        &item("web"),
        json_body(&put_body("web", &ip)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        cluster_ip(&out),
        Some(ip.as_str()),
        "unchanged, not reallocated"
    );
}

#[tokio::test]
async fn put_omitting_clusterip_defaults_from_stored_is_200() {
    // k8s update parity (golden `put-fresh`): a replace body that omits
    // clusterIP keeps the allocation — 200, the stored IP echoed back.
    let r = app();
    let ip = created_with_ip(&r, "web").await;
    let body = svc(
        "web",
        json!({ "type": "ClusterIP", "ports": [{ "port": 80 }] }),
    );
    let (st, out) = send(r, "PUT", &item("web"), json_body(&body)).await;
    assert_eq!(st, StatusCode::OK, "omitted clusterIP persists: {out}");
    assert_eq!(cluster_ip(&out), Some(ip.as_str()));
}

#[tokio::test]
async fn merge_patch_changing_clusterip_is_422() {
    let r = app();
    let _ip = created_with_ip(&r, "web").await;
    let patch = json!({ "spec": { "clusterIP": "10.43.77.77" } });
    let (st, out) = send(
        r.clone(),
        "PATCH",
        &item("web"),
        Some((
            "application/merge-patch+json",
            serde_json::to_vec(&patch).unwrap(),
        )),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(out["reason"], "Invalid");
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("may not change once set"),
        "{out}"
    );
}

#[tokio::test]
async fn strategic_patch_changing_clusterip_is_422() {
    let r = app();
    let _ip = created_with_ip(&r, "web").await;
    let patch = json!({ "spec": { "clusterIP": "10.43.77.78" } });
    let (st, _) = send(
        r,
        "PATCH",
        &item("web"),
        Some((
            "application/strategic-merge-patch+json",
            serde_json::to_vec(&patch).unwrap(),
        )),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

/// SSA apply with an explicit different clusterIP -> 422; leaving it out
/// -> 200 with the stored VIP preserved (the merge keeps self-owned live
/// fields the apply does not mention).
#[tokio::test]
async fn apply_changing_clusterip_is_422_leaving_it_is_200() {
    let r = app();
    let ip = created_with_ip(&r, "ssa").await;
    let changed = svc(
        "ssa",
        json!({ "ports": [{ "port": 81 }], "clusterIP": "10.43.66.66" }),
    );
    let (st, out) = send(
        r.clone(),
        "PATCH",
        &format!("{}?fieldManager=tester", item("ssa")),
        Some((
            "application/apply-patch+yaml",
            serde_json::to_vec(&changed).unwrap(),
        )),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("may not change once set"),
        "{out}"
    );
    let leave = svc("ssa", json!({ "ports": [{ "port": 81 }] }));
    let (st, out) = send(
        r.clone(),
        "PATCH",
        &format!("{}?fieldManager=tester", item("ssa")),
        Some((
            "application/apply-patch+yaml",
            serde_json::to_vec(&leave).unwrap(),
        )),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(cluster_ip(&out), Some(ip.as_str()), "apply keeps the VIP");
}

#[tokio::test]
async fn apply_create_still_allocates_and_conflicts() {
    // The create-via-apply branch runs the create-path judgment: allocate
    // when absent, 422 on an explicit VIP another Service already holds.
    let r = app();
    let ip = created_with_ip(&r, "vip-owner").await;
    let hit = svc(
        "ssa-new",
        json!({ "ports": [{ "port": 80 }], "clusterIP": ip }),
    );
    let (st, out) = send(
        r.clone(),
        "PUT",
        &format!("{}?fieldManager=tester", item("ssa-new")),
        Some((
            "application/apply-patch+yaml",
            serde_json::to_vec(&hit).unwrap(),
        )),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already allocated"),
        "cross-Service VIP conflict: {out}"
    );
    let bare = svc("ssa-new", json!({ "ports": [{ "port": 80 }] }));
    let (st, out) = send(
        r,
        "PUT",
        &format!("{}?fieldManager=tester", item("ssa-new")),
        Some((
            "application/apply-patch+yaml",
            serde_json::to_vec(&bare).unwrap(),
        )),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert!(
        in_default_cidr(cluster_ip(&out).expect("allocated")),
        "{out}"
    );
}

/// (e) The create-path VIP:port cross-Service conflict stays rejected —
/// the write path reuses the same judgment (a shared VIP implies a shared
/// VIP:port pair).
#[tokio::test]
async fn create_vip_conflict_rejected_and_reused_on_write() {
    let r = app();
    let a = svc(
        "a",
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.5.5" }),
    );
    let (st, _) = send(r.clone(), "POST", URI, json_body(&a)).await;
    assert_eq!(st, StatusCode::CREATED);
    let b = svc(
        "b",
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.5.5" }),
    );
    let (st, out) = send(r, "POST", URI, json_body(&b)).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("10.43.5.5"),
        "{out}"
    );
    // Write-path reuse of the same judgment: a stored Service that holds NO
    // clusterIP yet (a pre-Q44 legacy object seeded straight into the
    // store) cannot PUT itself onto a foreign VIP.
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let legacy = svc(
        "legacy",
        json!({ "type": "ClusterIP", "ports": [{ "port": 80 }] }),
    );
    store
        .create(
            &storage::Key::new("", "services", "default", "legacy"),
            legacy,
        )
        .await
        .unwrap();
    let r2 = app_seeded_with(store);
    let owner = svc(
        "owner",
        json!({ "ports": [{ "port": 80 }], "clusterIP": "10.43.5.5" }),
    );
    let (st, _) = send(r2.clone(), "POST", URI, json_body(&owner)).await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, out) = send(
        r2,
        "PUT",
        &item("legacy"),
        json_body(&put_body("legacy", "10.43.5.5")),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already allocated"),
        "cross-Service conflict on the write path: {out}"
    );
}

// --- S52-D0: nodePort matrix ----------------------------------------------

/// Create a NodePort Service with an explicit nodePort.
async fn created_nodeport(r: &axum::Router, name: &str, np: u64) {
    let body = svc(
        name,
        json!({ "type": "NodePort", "ports": [{ "port": 80, "nodePort": np }] }),
    );
    let (st, out) = send(r.clone(), "POST", URI, json_body(&body)).await;
    assert_eq!(st, StatusCode::CREATED, "{out}");
}

fn nodeport_put_body(name: &str, ip: &str, np: u64) -> Value {
    svc(
        name,
        json!({
            "type": "NodePort",
            "ports": [{ "port": 80, "nodePort": np }],
            "clusterIP": ip
        }),
    )
}

#[tokio::test]
async fn put_changing_set_nodeport_is_422() {
    let r = app();
    created_nodeport(&r, "np", 30400).await;
    let ip = {
        let (st, got) = send(r.clone(), "GET", &item("np"), None).await;
        assert_eq!(st, StatusCode::OK);
        cluster_ip(&got).expect("vip").to_string()
    };
    let (st, out) = send(
        r.clone(),
        "PUT",
        &item("np"),
        json_body(&nodeport_put_body("np", &ip, 30401)),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("30400"),
        "message names the stored port: {out}"
    );
}

#[tokio::test]
async fn put_keeping_nodeport_is_200() {
    let r = app();
    created_nodeport(&r, "np", 30402).await;
    let ip = {
        let (st, got) = send(r.clone(), "GET", &item("np"), None).await;
        assert_eq!(st, StatusCode::OK);
        cluster_ip(&got).expect("vip").to_string()
    };
    let (st, out) = send(
        r.clone(),
        "PUT",
        &item("np"),
        json_body(&nodeport_put_body("np", &ip, 30402)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(node_port(&out), Some(30402), "unchanged");
}

#[tokio::test]
async fn patch_changing_nodeport_is_422() {
    let r = app();
    created_nodeport(&r, "np", 30403).await;
    let patch = json!({ "spec": { "ports": [{ "port": 80, "nodePort": 30999 }] } });
    let (st, out) = send(
        r,
        "PATCH",
        &item("np"),
        Some((
            "application/strategic-merge-patch+json",
            serde_json::to_vec(&patch).unwrap(),
        )),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("30403"),
        "{out}"
    );
}

/// Old holds NO nodePort (ClusterIP-typed): the write-path validate branch
/// accepts a valid explicit nodePort and still rejects a foreign one.
#[tokio::test]
async fn nodeport_unset_old_validate_path_ok() {
    let r = app();
    created_nodeport(&r, "squatter", 30410).await;
    let ip = created_with_ip(&r, "plain").await;
    // Valid + free -> 200, kept verbatim.
    let (st, out) = send(
        r.clone(),
        "PUT",
        &item("plain"),
        json_body(&nodeport_put_body("plain", &ip, 30411)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{out}");
    assert_eq!(node_port(&out), Some(30411));
    // Taken by a DIFFERENT Service -> 422 (same message as create).
    let ip2 = created_with_ip(&r, "plain2").await;
    let (st, out) = send(
        r,
        "PUT",
        &item("plain2"),
        json_body(&nodeport_put_body("plain2", &ip2, 30410)),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already allocated"),
        "{out}"
    );
}
