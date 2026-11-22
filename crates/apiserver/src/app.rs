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

use axum::routing::{get, post, put};
use axum::Router;

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

/// Assemble the router over a ready [`AppState`] (shared with [`crate::serve`]).
///
/// T1.3a/b (Q47): when `state.auth` is armed the `authn` -> `authz`
/// middleware pair wraps every route (axum layers added later run earlier,
/// so authz is added first and authn runs first on the wire). The default
/// anonymous plane stays layer-free — identical bytes to pre-T1.3.
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

    if auth_armed {
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
    }
}
