//! In-process auth middleware tests (T1.3a/T1.3b, Q47): full router with
//! `auth` armed, exercised through `tower::ServiceExt::oneshot` — the 401
//! face, the 403 face, the authenticated-allow discovery posture, and the
//! `system:nodes` bootstrap chain.

#![cfg(test)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;

use storage::{EmbeddedStorage, StorageBackend};

use crate::app::api_app_full;
use crate::authn::TokenAuth;

fn token_file() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("initpro-auth-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("tokens.csv");
    std::fs::write(
        &p,
        "# header comment\n\
         admintok,admin,u1,\"system:masters\"\n\
         kubelet-n1,kubelet-node1,u2,\"system:nodes\"\n\
         pleb,pleb,u3\n",
    )
    .unwrap();
    p
}

async fn armed_app() -> axum::Router {
    let auth = Arc::new(TokenAuth::load(&token_file()).unwrap());
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    crate::authz_bootstrap::bootstrap_rbac(&store).await;
    api_app_full(
        Arc::new(api::SchemaRegistry::with_core_v1()),
        store,
        "127.0.0.1:6443".into(),
        None,
        Some(auth),
    )
}

async fn call(app: &axum::Router, auth: Option<&str>, path: &str) -> StatusCode {
    let mut req = Request::builder().uri(path).body(Body::empty()).unwrap();
    if let Some(tok) = auth {
        req.headers_mut()
            .insert("authorization", format!("Bearer {tok}").parse().unwrap());
    }
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn missing_or_unknown_token_is_401() {
    let app = armed_app().await;
    assert_eq!(
        call(&app, None, "/api/v1/namespaces").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, Some("nope"), "/api/v1/namespaces").await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn admin_token_passes_rbac() {
    let app = armed_app().await;
    assert_eq!(
        call(&app, Some("admintok"), "/api/v1/namespaces").await,
        StatusCode::OK
    );
    // create in the default namespace: cluster-admin wildcard.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/namespaces/default/configmaps")
        .header("authorization", "Bearer admintok")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"cm1"}}"#,
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn ungranted_token_is_403_but_discovery_allows() {
    let app = armed_app().await;
    // pleb holds no binding: resource requests are forbidden...
    assert_eq!(
        call(&app, Some("pleb"), "/api/v1/namespaces").await,
        StatusCode::FORBIDDEN
    );
    // ...but non-resource endpoints (discovery, /readyz) are
    // authenticated-allow (Q47: no nonResourceURL subset matching).
    for p in ["/api", "/apis", "/api/v1"] {
        assert_eq!(call(&app, Some("pleb"), p).await, StatusCode::OK, "{p}");
    }
    // and without any token they are still 401 (authn runs first).
    assert_eq!(call(&app, None, "/api/v1").await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn system_nodes_bootstrap_chain_grants_kubelet_face() {
    let app = armed_app().await;
    assert_eq!(
        call(&app, Some("kubelet-n1"), "/api/v1/namespaces/default/pods").await,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, Some("kubelet-n1"), "/api/v1/nodes").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn anonymous_router_without_auth_untouched() {
    // The default (auth = None) plane passes through unauthenticated —
    // the pre-T1.3 wire posture (golden G01-G04 pin).
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let app = api_app_full(
        Arc::new(api::SchemaRegistry::with_core_v1()),
        store,
        "127.0.0.1:6443".into(),
        None,
        None,
    );
    assert_eq!(call(&app, None, "/api/v1/namespaces").await, StatusCode::OK);
}
