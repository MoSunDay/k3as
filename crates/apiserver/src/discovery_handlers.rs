//! axum handlers for the API discovery endpoints (TODO **T1.2a**).
//!
//! Each handler is a thin transport wrapper over an [`api::discovery`]
//! pure function. The handlers own no business logic; they translate a
//! [`SchemaRegistry`] snapshot into an HTTP response body.
//!
//! Unknown group+version -> `404 Not Found` (matches upstream kube-apiserver,
//! which returns 404 for an unregistered `/apis/<g>/<v>` path).
//!
//! The shared [`AppState`] (schema + storage + server address) lives in
//! [`crate::state`]; this module owns only the discovery routes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{APIGroupList, APIResourceList, APIVersions};

use crate::state::AppState;

/// Build the discovery [`Router`] (mounted under the full [`crate::app::api_app`]).
pub(crate) fn discovery_routes() -> Router<AppState> {
    Router::new()
        .route("/api", get(core_api_versions_handler))
        .route("/api/v1", get(core_v1_resources_handler))
        .route("/apis", get(api_group_list_handler))
        .route("/apis/{group}/{version}", get(api_resources_handler))
}

/// `GET /api` -> core group served versions (`APIVersions`).
async fn core_api_versions_handler(State(st): State<AppState>) -> Json<APIVersions> {
    Json(api::core_api_versions(&st.registry, &st.server_address))
}

/// `GET /apis` -> all non-core groups (`APIGroupList`).
async fn api_group_list_handler(State(st): State<AppState>) -> Json<APIGroupList> {
    Json(api::api_group_list(&st.registry))
}

/// `GET /api/v1` -> core/v1 resource index.
async fn core_v1_resources_handler(State(st): State<AppState>) -> DiscoveryList {
    match api::api_resource_list(&st.registry, "", "v1") {
        Some(list) => DiscoveryList::Ok(Json(list)),
        None => DiscoveryList::NotFound,
    }
}

/// `GET /apis/{group}/{version}` -> resource index, or 404 if unregistered.
async fn api_resources_handler(
    State(st): State<AppState>,
    Path((group, version)): Path<(String, String)>,
) -> DiscoveryList {
    match api::api_resource_list(&st.registry, &group, &version) {
        Some(list) => DiscoveryList::Ok(Json(list)),
        None => DiscoveryList::NotFound,
    }
}

/// Response enum: either a JSON `APIResourceList` body or a bare 404.
enum DiscoveryList {
    Ok(Json<APIResourceList>),
    NotFound,
}

impl IntoResponse for DiscoveryList {
    fn into_response(self) -> axum::response::Response {
        match self {
            DiscoveryList::Ok(json) => json.into_response(),
            DiscoveryList::NotFound => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The HTTP-level behaviour is exercised end-to-end in
    //! `tests/discovery_http.rs` via `api_app` + axum's in-process transport.
    use crate::app::api_app;
    use std::sync::Arc;
    use api::SchemaRegistry;
    use storage::EmbeddedStorage;

    #[test]
    fn api_app_builds_for_served_schema() {
        let mut reg = SchemaRegistry::with_core_v1();
        api::initpro::register(&mut reg);
        let store: Arc<dyn storage::StorageBackend> = Arc::new(EmbeddedStorage::new());
        let _router = api_app(Arc::new(reg), store, "127.0.0.1:6443".to_string());
    }
}
