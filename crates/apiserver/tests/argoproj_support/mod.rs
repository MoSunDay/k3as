//! Shared support for the argoproj.io integration tests (T7.3 Scope A).
//!
//! Split out of `argoproj_crud.rs` when that file hit the 400-line limit:
//! the fixtures and request helpers below are consumed by BOTH test
//! binaries: `argoproj_crud.rs` (CRUD + discovery) and `argoproj_ssa.rs`
//! (SSA + verbatim storage + generateName). Single-consumer helpers stay
//! in their consuming file.
//!
//! Harness mirrors `rest_crud.rs` / `rest_apply.rs` (real axum handlers
//! over a fresh `EmbeddedStorage`, in-process `oneshot`, no socket; ADR
//! Q17).; the registry matches the real server wiring
//! (`crates/cli/src/discovery.rs`).

use std::sync::Arc;

use api::SchemaRegistry;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, StorageBackend};
use tower::ServiceExt;

pub const GV: &str = "/apis/argoproj.io/v1alpha1";

pub fn served() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    api::initpro::register(&mut reg);
    api::argoproj::register(&mut reg);
    reg
}

/// One router over a fresh embedded store (state is Arc-shared, so cloning
/// the router keeps the same store across requests).
pub fn app() -> axum::Router {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    apiserver::api_app(Arc::new(served()), store, "127.0.0.1:6443".to_string())
}

pub async fn send(
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

pub fn json_body(v: &Value) -> Option<(&'static str, Vec<u8>)> {
    Some(("application/json", serde_json::to_vec(v).unwrap()))
}

pub fn ns_path(ns: &str, plural: &str) -> String {
    format!("{GV}/namespaces/{ns}/{plural}")
}

pub fn item_path(ns: &str, plural: &str, name: &str) -> String {
    format!("{}/{}", ns_path(ns, plural), name)
}

pub async fn create(r: axum::Router, ns: &str, plural: &str, body: &Value) -> (StatusCode, Value) {
    send(r, "POST", &ns_path(ns, plural), json_body(body)).await
}

pub async fn get(r: axum::Router, ns: &str, plural: &str, name: &str) -> (StatusCode, Value) {
    send(r, "GET", &item_path(ns, plural, name), None).await
}

/// PATCH with an arbitrary content-type + JSON body (generic patch / SSA).
pub async fn patch(r: axum::Router, uri: &str, ct: &str, body: &Value) -> (StatusCode, Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    send(r, "PATCH", uri, Some((ct, bytes))).await
}

/// The two served kinds, each with one representative body.
pub fn kinds() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("workflows", "Workflow", workflow("wf-main")),
        ("cronworkflows", "CronWorkflow", cron_workflow("cwf-main")),
    ]
}

/// Representative Argo Workflows v4 `Workflow`: `entrypoint` + a `steps`
/// template delegating to a real-image container template, plus
/// `arguments.parameters` and a controller-written `.status.phase` that must
/// round-trip verbatim.
pub fn workflow(name: &str) -> Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Workflow",
        "metadata": { "name": name, "namespace": "default" },
        "spec": {
            "entrypoint": "main",
            "arguments": { "parameters": [{ "name": "message", "value": "hello-argo" }] },
            "templates": [
                { "name": "main", "steps": [[{ "name": "say", "template": "echo" }]] },
                { "name": "echo", "container": {
                    "name": "main", "image": "busybox:1.36",
                    "command": ["echo"], "args": ["{{workflow.parameters.message}}"]
                } }
            ]
        },
        "status": { "phase": "Running" }
    })
}

/// Representative v4 `CronWorkflow`: `workflowSpec` with one container
/// template, `schedules` (the v4 field that replaced v3 `schedule`) and
/// `concurrencyPolicy: Forbid`.
pub fn cron_workflow(name: &str) -> Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "CronWorkflow",
        "metadata": { "name": name, "namespace": "default" },
        "spec": {
            "schedules": ["*/5 * * * *"],
            "concurrencyPolicy": "Forbid",
            "workflowSpec": {
                "entrypoint": "tick",
                "templates": [{ "name": "tick", "container": {
                    "name": "main", "image": "busybox:1.36", "command": ["date"]
                } }]
            }
        }
    })
}
