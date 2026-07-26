//! axum handlers for the API discovery endpoints (TODO **T1.2a**).
//!
//! Each handler is a thin transport wrapper over a [`api::discovery`]
//! pure function. The handlers own no business logic; they translate a
//! [`SchemaRegistry`] snapshot into an HTTP response body.
//!
//! Unknown group+version -> `404 Not Found` (matches upstream kube-apiserver,
//! which returns 404 for an unregistered `/apis/<g>/<v>` path).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use api::SchemaRegistry;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{APIGroupList, APIResourceList, APIVersions};

/// Shared, read-only application state: the served schema + the advertised
/// server address (surfaced in the `/api` `APIVersions` document). Pure data;
/// cloned cheaply across tasks via the inner `Arc`.
#[derive(Clone)]
pub(crate) struct AppState {
    registry: Arc<SchemaRegistry>,
    server_address: String,
}

/// Build the discovery [`Router`] for a served schema.
///
/// This is the single wiring point used by both [`crate::serve`] (production)
/// and the in-process tests: registry + advertised address -> routes.
pub fn discovery_app(registry: SchemaRegistry, server_address: impl Into<String>) -> Router {
    let state = AppState {
        registry: Arc::new(registry),
        server_address: server_address.into(),
    };
    Router::new()
        .route("/api", get(core_api_versions_handler))
        .route("/api/v1", get(core_v1_resources_handler))
        .route("/apis", get(api_group_list_handler))
        .route("/apis/{group}/{version}", get(api_resources_handler))
        .with_state(state)
}

/// `GET /api` -> core group served versions (`APIVersions`).
async fn core_api_versions_handler(State(st): State<AppState>) -> Json<APIVersions> {
    Json(api::core_api_versions(&st.registry, &st.server_address))
}

/// `GET /apis` -> all non-core groups (`APIGroupList`).
async fn api_group_list_handler(State(st): State<AppState>) -> Json<APIGroupList> {
    Json(api::api_group_list(&st.registry))
}

/// `GET /api/v1` -> core/v1 resource index (`APIResourceList`), or 404.
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
    //! `tests/discovery_http.rs` via `discovery_app` + axum's in-process
    //! transport. Byte fidelity of the bodies lives in `api`.

    use super::*;

    #[test]
    fn discovery_app_builds_for_served_schema() {
        let mut reg = SchemaRegistry::with_core_v1();
        api::initpro::register(&mut reg);
        let _router = discovery_app(reg, "127.0.0.1:6443");
    }
}
