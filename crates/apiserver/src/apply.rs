//! Server-Side Apply HTTP handler — TODO **T1.2c**.
//!
//! Dispatched from the PUT/PATCH item handlers when Content-Type is
//! `application/apply-patch+yaml`. Delegates the core algorithm to
//! [`api::apply`].
//!
//! Note: only JSON bodies are accepted in this sprint (the `+yaml` suffix in
//! the content-type is for kubectl wire-compatibility; YAML parsing is
//! deferred to a later TODO).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use api::apply::{apply_object, set_managed_fields, ApplyOptions};
use api::patch::PatchStrategy;

use crate::error::{storage_error, ApiError};
use crate::state::{item_key, resolve, set_namespace, set_resource_version, set_type_meta, AppState, Loc};

/// Query parameters for server-side apply (`?fieldManager=&force=`).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ApplyQuery {
    #[serde(default, rename = "fieldManager")]
    pub field_manager: Option<String>,
    #[serde(default)]
    pub force: Option<bool>,
    #[serde(default, rename = "fieldValidation")]
    #[allow(dead_code)]
    pub field_validation: Option<String>,
}

/// Returns true when the content-type indicates a server-side apply request.
pub(crate) fn is_apply_ct(ct: &str) -> bool {
    ct.contains("apply-patch")
}

/// Execute server-side apply for a single item.
pub(crate) async fn do_apply(
    st: &AppState,
    loc: &Loc,
    name: &str,
    mut body: Value,
    query: &ApplyQuery,
) -> Response {
    let res = match resolve(&st.registry, &loc.group, &loc.version, &loc.resource) {
        Some(r) => r,
        None => return ApiError::NotFoundResource.into_response(),
    };
    if res.scope.is_namespaced() != loc.namespace.is_some() {
        return ApiError::NotFoundResource.into_response();
    }

    let key = item_key(loc, &res, name);
    let field_manager = query.field_manager.as_deref().unwrap_or("init-pro");
    let force = query.force.unwrap_or(false);

    // Normalise the desired body: stamp name, namespace, type meta.
    set_object_name(&mut body, name);
    if let Some(ns) = &loc.namespace {
        set_namespace(&mut body, ns);
    }
    set_type_meta(&mut body, &res.api_version, &res.kind);

    // Read live object.
    let live_entry = match st.store.get(&key).await {
        Ok(opt) => opt,
        Err(e) => return storage_error(e, &res.kind, name).into_response(),
    };

    let opts = ApplyOptions {
        field_manager: field_manager.to_string(),
        force,
        api_version: res.api_version.clone(),
        time: None, // RFC-3339 timestamp deferred
    };
    let strategy = PatchStrategy::kubernetes_defaults();
    let result = apply_object(live_entry.as_ref().map(|e| &e.value), &body, &opts, &strategy);

    // Conflict -> 409.
    if !result.conflicts.is_empty() {
        return ApiError::ApplyConflict {
            kind: res.kind,
            name: name.to_string(),
            conflicts: result
                .conflicts
                .iter()
                .map(|c| (c.path.clone(), c.manager.clone()))
                .collect(),
        }
        .into_response();
    }

    let mut value = result.value;
    set_type_meta(&mut value, &res.api_version, &res.kind);
    set_managed_fields(&mut value, result.managed_fields);

    if result.created {
        match st.store.create(&key, value).await {
            Ok(entry) => {
                let mut v = entry.value;
                set_resource_version(&mut v, entry.mod_revision);
                (StatusCode::CREATED, Json(v)).into_response()
            }
            Err(e) => storage_error(e, &res.kind, name).into_response(),
        }
    } else {
        let if_revision = live_entry.as_ref().map(|e| e.mod_revision);
        match st.store.update(&key, value, if_revision).await {
            Ok(entry) => {
                let mut v = entry.value;
                set_resource_version(&mut v, entry.mod_revision);
                (StatusCode::OK, Json(v)).into_response()
            }
            Err(e) => storage_error(e, &res.kind, name).into_response(),
        }
    }
}

/// Set `metadata.name` on a JSON object (creating metadata if absent).
fn set_object_name(value: &mut Value, name: &str) {
    if let Some(obj) = value.as_object_mut() {
        let meta = obj
            .entry("metadata")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(m) = meta.as_object_mut() {
            m.insert("name".into(), Value::String(name.to_string()));
        }
    }
}
