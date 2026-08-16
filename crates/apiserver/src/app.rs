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

use axum::routing::{get, post};
use axum::Router;

use crate::collection;
use crate::discovery_handlers::discovery_routes;
use crate::item;
use crate::state::AppState;
use api::SchemaRegistry;
use storage::StorageBackend;

/// Build the full API [`Router`] (discovery + CRUD + watch) from its parts.
pub fn api_app(
    registry: Arc<SchemaRegistry>,
    store: Arc<dyn StorageBackend>,
    server_address: String,
) -> Router {
    router(AppState {
        registry,
        store,
        server_address,
    })
}

/// Assemble the router over a ready [`AppState`] (shared with [`crate::serve`]).
pub(crate) fn router(state: AppState) -> Router {
    Router::new()
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
            get(item::core_get)
                .put(item::core_replace)
                .delete(item::core_delete)
                .patch(item::core_patch),
        )
        // T3.2: pod binding subresource (upstream `pods/binding`; the
        // in-process scheduler drives the same do_bind semantics).
        .route(
            "/api/v1/namespaces/{namespace}/pods/{name}/binding",
            post(crate::binding::bind_pod),
        )
        .route(
            "/api/v1/namespaces/{namespace}/{resource}/{name}",
            get(item::core_get_ns)
                .put(item::core_replace_ns)
                .delete(item::core_delete_ns)
                .patch(item::core_patch_ns),
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
            get(item::grp_get)
                .put(item::grp_replace)
                .delete(item::grp_delete)
                .patch(item::grp_patch),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}/{name}",
            get(item::grp_get_ns)
                .put(item::grp_replace_ns)
                .delete(item::grp_delete_ns)
                .patch(item::grp_patch_ns),
        )
        .with_state(state)
}
