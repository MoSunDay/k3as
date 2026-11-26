//! Router assembly: the full discovery + REST CRUD/watch [`Router`] (T1.2).
//!
//! [`api_app`] wires the discovery routes (T1.2a) and the REST collection +
//! per-item routes (T1.2b) over a shared [`AppState`]. It is the single
//! wiring point used by both [`crate::serve`] (production) and the in-process
//! tests.
//!
//! Route layout (axum 0.8 path params — static segments win over params, and
//! `/namespaces/<ns>/...` has a distinct segment count so it never collides):
//!  - core group `""`:   `/api/v1/<r>` + `/api/v1/namespaces/<ns>/<r>`
//!  - grouped:           `/apis/<g>/<v>/<r>` + `/apis/<g>/<v>/namespaces/<ns>/<r>`

use std::sync::Arc;
use std::time::Duration;

use axum::error_handling::HandleErrorLayer;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{BoxError, Router};
use tower::limit::GlobalConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::timeout::TimeoutLayer;
use tower::ServiceBuilder;

use crate::collection;
use crate::discovery_handlers::discovery_routes;
use crate::item_http;
use crate::state::AppState;
use api::SchemaRegistry;
use storage::StorageBackend;

/// Build the full API [`Router`] (discovery + CRUD + watch) from its parts.
///
/// Anyhome is OFF (`workspace_nfs: None`): the per-Pod anyhome defaulting
/// (T7.5, Q43) is inert — the historical single-argument behavior every
/// pre-T7.5 test pins. Production wiring uses [`api_app_with_workspace_nfs`].
pub fn api_app(
    registry: Arc<SchemaRegistry>,
    store: Arc<dyn StorageBackend>,
    server_address: String,
) -> Router {
    api_app_with_workspace_nfs(registry, store, server_address, None)
}

/// [`api_app`] with the anyhome NFS endpoint threaded in (T7.5, Q43):
/// `Some(addr)` turns the `init-pro.io/anyhome(-mounts)` Pod annotations
/// into injected nfs volumes + mounts at create/apply time.
pub fn api_app_with_workspace_nfs(
    registry: Arc<SchemaRegistry>,
    store: Arc<dyn StorageBackend>,
    server_address: String,
    workspace_nfs: Option<String>,
) -> Router {
    api_app_full(registry, store, server_address, workspace_nfs, None)
}

/// [`api_app_with_workspace_nfs`] + static-token auth (T1.3a, Q47):
/// `Some(auth)` layers `authn` -> `authz` middleware (the anonymous default
/// stays layer-free so the pre-T1.3 wire behavior is byte-identical).
pub fn api_app_full(
    registry: Arc<SchemaRegistry>,
    store: Arc<dyn StorageBackend>,
    server_address: String,
    workspace_nfs: Option<String>,
    auth: Option<Arc<crate::authn::TokenAuth>>,
) -> Router {
    let state = AppState {
        registry,
        store,
        server_address,
        workspace_nfs,
        auth,
        // T4.3 Scope C (Q44): the test-facing constructor allocates from
        // the k3s default pool; production threads `--service-cidr` through
        // `serve`.
        service_cidr: crate::service_ip::ServiceCidr::default(),
        // T4.5 (Q65): default-on reverse relay tunnel registry.
        tunnels: crate::tunnel_registry::TunnelRegistry::new_shared(),
    };
    router(state)
}

/// A7: request deadline, outermost over the whole router (upstream
/// `--min-request-timeout` parity). Long enough that watch streams and the
/// reverse-relay tunnel park survive; `TimeoutLayer` bounds time-to-response
/// only, and a watch handler hands back its chunked body (Q10) immediately.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);

/// A7: in-flight request cap (upstream `--max-requests-in-flight` shape).
pub(crate) const MAX_IN_FLIGHT: usize = 1024;

/// A7: a request that blew the deadline answers `504 GATEWAY_TIMEOUT`.
fn timeout_failure(err: BoxError) -> std::future::Ready<axum::response::Response> {
    tracing::debug!(error = %err, "apiserver request deadline exceeded");
    std::future::ready(crate::error::status_failure(
        StatusCode::GATEWAY_TIMEOUT,
        "Timeout",
        "request exceeded the apiserver request timeout".to_string(),
    ))
}

/// A7: a request shed at the in-flight cap answers `503 SERVICE_UNAVAILABLE`.
fn overload_failure(err: BoxError) -> std::future::Ready<axum::response::Response> {
    tracing::debug!(error = %err, "apiserver shed an overloaded request");
    std::future::ready(crate::error::status_failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "ServiceUnavailable",
        "server is overloaded: max in-flight requests reached".to_string(),
    ))
}

/// A7: wrap `routes` in the deadline + concurrency-cap layers, OUTERMOST of
/// everything (`authn`/`authz` included).
///
/// [`GlobalConcurrencyLimitLayer`], not [`tower::limit::ConcurrencyLimitLayer`]:
/// axum's `Router::layer` maps the layer over EVERY route endpoint, and the
/// plain layer mints a fresh semaphore per service, so the cap would silently
/// become per-route instead of a global in-flight budget. The global layer
/// clones one `Arc<Semaphore>`.
///
/// Each `HandleErrorLayer` must sit outside its fallible layer in the SAME
/// `ServiceBuilder` stack: axum's [`Router::layer`] requires an infallible
/// service, so `Timeout`/`LoadShed` errors have to be folded into a response
/// before the next `Router::layer` boundary. `ServiceBuilder` applies its
/// first layer outermost, and a later `Router::layer` wraps the earlier ones,
/// so the deadline ends up outside the cap (a queued request still carries a
/// deadline). `max_in_flight` is a parameter only so the test can pin the
/// 503 path; production passes [`MAX_IN_FLIGHT`].
fn hardened(routes: Router, max_in_flight: usize) -> Router {
    routes
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(overload_failure))
                .layer(LoadShedLayer::new())
                .layer(GlobalConcurrencyLimitLayer::new(max_in_flight)),
        )
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(timeout_failure))
                .layer(TimeoutLayer::new(REQUEST_TIMEOUT)),
        )
}

/// Assemble the router over a ready [`AppState`] (shared with [`crate::serve`]).
///
/// T1.3a/b (Q47): when `state.auth` is armed the `authn` -> `authz`
/// middleware pair wraps every route (axum layers added later run earlier,
/// so authz is added first and authn runs first on the wire). The default
/// anonymous plane stays layer-free — identical bytes to pre-T1.3.
///
/// A7: the assembled routes (auth pair included) are then wrapped by
/// [`hardened`] -- deadline and in-flight cap are outermost on the wire.
pub(crate) fn router(state: AppState) -> Router {
    let auth_armed = state.auth.is_some();
    let routes = Router::new()
        .merge(discovery_routes())
        // --- core group (`""`) ---
        .route(
            "/api/v1/{resource}",
            get(collection::core_list).post(collection::core_create),
        )
        .route(
            "/api/v1/namespaces/{namespace}/{resource}",
            get(collection::core_list_ns).post(collection::core_create_ns),
        )
        .route(
            "/api/v1/{resource}/{name}",
            get(item_http::core_get)
                .put(item_http::core_replace)
                .delete(item_http::core_delete)
                .patch(item_http::core_patch),
        )
        // T3.2: pod binding subresource (upstream `pods/binding`; the
        // in-process scheduler drives the same do_bind semantics).
        .route(
            "/api/v1/namespaces/{namespace}/pods/{name}/binding",
            post(crate::binding::bind_pod),
        )
        // T4.2: pod status subresource (upstream `pods/status`; the kubelet
        // PUTs the full pod — only `.status` is writable through this route).
        .route(
            "/api/v1/namespaces/{namespace}/pods/{name}/status",
            put(crate::pod_status::put_pod_status),
        )
        // Node status subresource (upstream `nodes/status`): the status
        // write path for heartbeats/capacity (cluster-scoped, `.status`
        // writable only). PUT = full-object write, PATCH = standard CTs.
        .route(
            "/api/v1/nodes/{name}/status",
            put(crate::node_status::put_node_status).patch(crate::node_status::patch_node_status),
        )
        // T4.5 (Q65): the agent-dialed reverse relay tunnel surface. These
        // five/six-segment paths never collide with the item routes above;
        // auth rides the same authn/authz middleware pair (Q47).
        .route(
            "/api/v1/nodes/{name}/tunnel/register",
            post(crate::tunnel_http::register),
        )
        .route(
            "/api/v1/nodes/{name}/tunnel/next",
            get(crate::tunnel_http::next),
        )
        .route(
            "/api/v1/nodes/{name}/tunnel/responses/{id}",
            post(crate::tunnel_http::respond),
        )
        // T4.2 Scope C: pods/log + pods/exec relay subresources — resolved
        // to the owning node's kubelet, then passed through verbatim (Q31).
        .route(
            "/api/v1/namespaces/{namespace}/pods/{name}/log",
            get(crate::pod_io::pod_log),
        )
        .route(
            "/api/v1/namespaces/{namespace}/pods/{name}/exec",
            post(crate::pod_io::pod_exec),
        )
        .route(
            "/api/v1/namespaces/{namespace}/{resource}/{name}",
            get(item_http::core_get_ns)
                .put(item_http::core_replace_ns)
                .delete(item_http::core_delete_ns)
                .patch(item_http::core_patch_ns),
        )
        // --- grouped groups ---
        .route(
            "/apis/{group}/{version}/{resource}",
            get(collection::grp_list).post(collection::grp_create),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}",
            get(collection::grp_list_ns).post(collection::grp_create_ns),
        )
        .route(
            "/apis/{group}/{version}/{resource}/{name}",
            get(item_http::grp_get)
                .put(item_http::grp_replace)
                .delete(item_http::grp_delete)
                .patch(item_http::grp_patch),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}/{name}",
            get(item_http::grp_get_ns)
                .put(item_http::grp_replace_ns)
                .delete(item_http::grp_delete_ns)
                .patch(item_http::grp_patch_ns),
        )
        .with_state(state.clone());

    let routes = if auth_armed {
        routes
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::authz::middleware,
            ))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::authn::middleware,
            ))
    } else {
        routes
    };
    hardened(routes, MAX_IN_FLIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::response::IntoResponse;
    use serde_json::Value;
    use tower::ServiceExt;

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    /// A7: the production constants are the documented parity values.
    #[test]
    fn deadline_and_cap_constants() {
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(1800));
        assert_eq!(MAX_IN_FLIGHT, 1024);
    }

    /// A7: an ordinary request passes through both layers untouched.
    #[tokio::test]
    async fn hardened_router_serves_normal_requests() {
        let routes = Router::new().route("/ok", get(|| async { "ok" }));
        let app = hardened(routes, MAX_IN_FLIGHT);
        let resp = app
            .oneshot(Request::builder().uri("/ok").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// A7: with the in-flight cap saturated the router sheds immediately with
    /// a `503 SERVICE_UNAVAILABLE` Status body instead of queueing forever.
    #[tokio::test]
    async fn saturated_cap_sheds_with_service_unavailable() {
        let routes = Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    "slow"
                }),
            )
            .route("/fast", get(|| async { "fast" }));
        let app = hardened(routes, 1);
        let holder = {
            let app = app.clone();
            tokio::spawn(async move {
                app.oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            })
        };
        // Let the first request take the only permit.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let resp = app
            .oneshot(Request::builder().uri("/fast").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(body["kind"], "Status");
        assert_eq!(body["reason"], "ServiceUnavailable");
        assert_eq!(body["code"], 503);
        // The in-flight request is unaffected by the shed.
        let resp = holder.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// A7: both error mappers emit the canonical `metav1.Status` envelope.
    #[tokio::test]
    async fn error_mappers_emit_status_envelopes() {
        let err: BoxError = "boom".into();
        let resp = timeout_failure(err).await.into_response();
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = body_json(resp).await;
        assert_eq!(body["reason"], "Timeout");
        assert_eq!(body["code"], 504);
        let err: BoxError = "overloaded".into();
        let resp = overload_failure(err).await.into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(body["reason"], "ServiceUnavailable");
        assert_eq!(body["code"], 503);
    }
}
