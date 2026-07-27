//! HTTP-level discovery fidelity (TODO **T1.2a**).
//!
//! Mirrors the `api/tests/json_fidelity.rs` pattern: assert the wire
//! bytes served over HTTP are byte-correct Kubernetes `meta/v1` discovery
//! documents. We exercise the real axum handlers via [`discovery_app`] + axum's
//! in-process oneshot transport (no socket, no network egress), then assert each
//! endpoint's body equals the document produced by the pure builder, and that an
//! unknown group/version yields `404`.

use api::SchemaRegistry;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// A served schema identical to what `init-pro server` exposes: core/v1 native
/// types + the `init-pro.io/v1` group.
fn served_registry() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    api::initpro::register(&mut reg);
    reg
}

async fn body_string(resp: axum::http::Response<Body>) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn api_endpoint_serves_core_versions() {
    let reg = served_registry();
    let expected = serde_json::to_string(&api::core_api_versions(&reg, "127.0.0.1:6443")).unwrap();

    let app = apiserver::discovery_app(reg, "127.0.0.1:6443");
    let resp = app
        .oneshot(Request::builder().uri("/api").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(
        body_string(resp).await,
        expected,
        "/api body must be byte-identical"
    );
}

#[tokio::test]
async fn apis_endpoint_serves_group_list() {
    let reg = served_registry();
    let expected = serde_json::to_string(&api::api_group_list(&reg)).unwrap();

    let app = apiserver::discovery_app(reg, "127.0.0.1:6443");
    let resp = app
        .oneshot(Request::builder().uri("/apis").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_eq!(body, expected, "/apis body must be byte-identical");
    assert!(
        body.contains(r#""init-pro.io""#),
        "group list must include init-pro.io"
    );
}

#[tokio::test]
async fn api_v1_serves_core_resource_list() {
    let reg = served_registry();
    let expected = serde_json::to_string(&api::api_resource_list(&reg, "", "v1").unwrap()).unwrap();

    let app = apiserver::discovery_app(reg, "127.0.0.1:6443");
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_eq!(body, expected, "/api/v1 byte-identical");
    assert!(body.contains(r#""pods""#) && body.contains(r#""namespaces""#));
}

#[tokio::test]
async fn apis_initpro_v1_serves_resource_list() {
    let reg = served_registry();
    let expected =
        serde_json::to_string(&api::api_resource_list(&reg, "init-pro.io", "v1").unwrap()).unwrap();

    let app = apiserver::discovery_app(reg, "127.0.0.1:6443");
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/apis/init-pro.io/v1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_eq!(body, expected);
    assert!(body.contains(r#""luarouters""#));
}

#[tokio::test]
async fn unknown_group_version_returns_404() {
    let reg = served_registry();
    let app = apiserver::discovery_app(reg, "127.0.0.1:6443");
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/apis/fabricated.io/v9beta1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
