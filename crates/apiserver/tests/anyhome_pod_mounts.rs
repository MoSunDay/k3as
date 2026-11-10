//! T7.5 (Q43) — per-Pod anyhome NFS mount defaulting, wire-level.
//!
//! Mirrors `rest_crud.rs` / `rest_apply.rs`: the real axum router via
//! in-process `oneshot`, driven with `api_app_with_workspace_nfs` (anyhome
//! plane ON) and plain `api_app` (plane OFF), against a fresh
//! `EmbeddedStorage` per test (Q17).

use std::sync::Arc;

use api::SchemaRegistry;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, StorageBackend};
use tower::ServiceExt;

const NFS: &str = "127.0.0.1:11111";

/// Router with the anyhome plane ON (`--workspace-nfs` set).
fn anyhome_app() -> axum::Router {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    apiserver::api_app_with_workspace_nfs(
        Arc::new(SchemaRegistry::with_core_v1()),
        store,
        "127.0.0.1:6443".to_string(),
        Some(NFS.to_string()),
    )
}

/// Router with the anyhome plane OFF (`--disable-anyhome`).
fn plain_app() -> axum::Router {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    apiserver::api_app(
        Arc::new(SchemaRegistry::with_core_v1()),
        store,
        "127.0.0.1:6443".to_string(),
    )
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

/// Pod body; `annotations` / `mounts` are `Value::Null` to omit the keys.
fn pod(name: &str, annotations: Value, mounts: Value) -> Value {
    let mut metadata = json!({ "name": name, "namespace": "default" });
    if !annotations.is_null() {
        metadata["annotations"] = annotations;
    }
    let mut container = json!({ "name": "app", "image": "busybox" });
    if !mounts.is_null() {
        container["volumeMounts"] = mounts;
    }
    json!({
        "apiVersion": "v1", "kind": "Pod", "metadata": metadata,
        "spec": { "containers": [container] },
    })
}

/// Server-side apply exactly the `rest_apply.rs` way: PATCH with the
/// `application/apply-patch+yaml` content type + `?fieldManager=`.
async fn apply_req(router: axum::Router, name: &str, body: &Value) -> (StatusCode, Value) {
    send(
        router,
        "PATCH",
        &format!("/api/v1/namespaces/default/pods/{name}?fieldManager=t75-mgr"),
        Some((
            "application/apply-patch+yaml",
            serde_json::to_vec(body).unwrap(),
        )),
    )
    .await
}

#[tokio::test]
async fn create_rw_pod_gets_workspace_volume_and_mount() {
    let r = anyhome_app();
    let (st, _) = send(
        r.clone(),
        "POST",
        "/api/v1/namespaces/default/pods",
        json_body(&pod(
            "p-rw",
            json!({ "init-pro.io/anyhome": "rw" }),
            Value::Null,
        )),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, got) = send(r, "GET", "/api/v1/namespaces/default/pods/p-rw", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        got["spec"]["volumes"],
        json!([{
            "name": "anyhome",
            "nfs": { "server": NFS, "path": "/pod/default/p-rw", "readOnly": false },
        }])
    );
    assert_eq!(
        got["spec"]["containers"][0]["volumeMounts"],
        json!([{ "name": "anyhome", "mountPath": "/workspace" }])
    );
}

#[tokio::test]
async fn create_ro_pod_marks_volume_read_only() {
    let r = anyhome_app();
    let (st, got) = send(
        r,
        "POST",
        "/api/v1/namespaces/default/pods",
        json_body(&pod(
            "p-ro",
            json!({ "init-pro.io/anyhome": "ro" }),
            Value::Null,
        )),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(
        got["spec"]["volumes"][0]["nfs"],
        json!({
            "server": NFS, "path": "/pod/default/p-ro", "readOnly": true,
        })
    );
}

#[tokio::test]
async fn create_pod_with_extra_mounts_annotation() {
    let r = anyhome_app();
    let (st, got) = send(
        r,
        "POST",
        "/api/v1/namespaces/default/pods",
        json_body(&pod(
            "p-mounts",
            json!({ "init-pro.io/anyhome-mounts": "/team/shared:/shared:ro,/cache:/cache" }),
            Value::Null,
        )),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(
        got["spec"]["volumes"],
        json!([
            { "name": "anyhome-shared",
              "nfs": { "server": NFS, "path": "/team/shared", "readOnly": true } },
            { "name": "anyhome-cache",
              "nfs": { "server": NFS, "path": "/cache", "readOnly": false } },
        ])
    );
    let mounts = got["spec"]["containers"][0]["volumeMounts"]
        .as_array()
        .unwrap();
    let paths: Vec<&str> = mounts
        .iter()
        .filter_map(|m| m["mountPath"].as_str())
        .collect();
    assert_eq!(paths, vec!["/shared", "/cache"]);
}

#[tokio::test]
async fn create_unannotated_pod_has_no_volumes() {
    let r = anyhome_app();
    let (st, got) = send(
        r,
        "POST",
        "/api/v1/namespaces/default/pods",
        json_body(&pod("p-plain", Value::Null, Value::Null)),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert!(got["spec"].get("volumes").is_none(), "byte-stable surface");
}

#[tokio::test]
async fn annotations_inert_when_anyhome_disabled() {
    let r = plain_app();
    let (st, got) = send(
        r,
        "POST",
        "/api/v1/namespaces/default/pods",
        json_body(&pod(
            "p-off",
            json!({ "init-pro.io/anyhome": "rw" }),
            Value::Null,
        )),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert!(got["spec"].get("volumes").is_none());
}

#[tokio::test]
async fn invalid_mode_annotation_rejected_422() {
    let r = anyhome_app();
    let (st, got) = send(
        r,
        "POST",
        "/api/v1/namespaces/default/pods",
        json_body(&pod(
            "p-bad",
            json!({ "init-pro.io/anyhome": "banana" }),
            Value::Null,
        )),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(got["reason"], "Invalid");
}

#[tokio::test]
async fn ssa_apply_injects_and_stays_idempotent() {
    let r = anyhome_app();
    let body = pod("p-ssa", json!({ "init-pro.io/anyhome": "rw" }), Value::Null);
    // First apply creates WITH the injected volume.
    let (s1, resp) = apply_req(r.clone(), "p-ssa", &body).await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(resp["spec"]["volumes"][0]["name"], json!("anyhome"));
    // Second apply of the SAME body: 200, still exactly one anyhome volume
    // and one /workspace mount (no duplication, no SSA strip).
    let (s2, resp) = apply_req(r.clone(), "p-ssa", &body).await;
    assert_eq!(s2, StatusCode::OK);
    let volumes = resp["spec"]["volumes"].as_array().expect("volumes");
    assert_eq!(volumes.iter().filter(|v| v["name"] == "anyhome").count(), 1);
    let mounts = resp["spec"]["containers"][0]["volumeMounts"]
        .as_array()
        .expect("volumeMounts");
    assert_eq!(
        mounts
            .iter()
            .filter(|m| m["mountPath"] == "/workspace")
            .count(),
        1
    );
    // And the STORED object agrees.
    let (st, got) = send(r, "GET", "/api/v1/namespaces/default/pods/p-ssa", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(got["spec"]["volumes"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn user_declared_mount_wins_over_injection() {
    let r = anyhome_app();
    let (st, got) = send(
        r,
        "POST",
        "/api/v1/namespaces/default/pods",
        json_body(&pod(
            "p-a4",
            json!({ "init-pro.io/anyhome-mounts": "/team/shared:/shared:ro" }),
            json!([{ "name": "user-vol", "mountPath": "/shared" }]),
        )),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    // No second mount at /shared: the user declaration wins.
    let mounts = got["spec"]["containers"][0]["volumeMounts"]
        .as_array()
        .unwrap();
    assert_eq!(
        mounts
            .iter()
            .filter(|m| m["mountPath"] == "/shared")
            .count(),
        1
    );
    assert_eq!(mounts[0]["name"], json!("user-vol"));
}
