//! REST per-item handlers: get / replace / delete / patch / apply (T1.2b + T1.2c).
//!
//! Routes (core group `""` + grouped):
//!  - `GET    /<collection>/<name>` -> read one object
//!  - `PUT    /<collection>/<name>` -> replace (honours `metadata.resourceVersion` CAS)
//!    or server-side apply when Content-Type: application/apply-patch+yaml
//!  - `DELETE /<collection>/<name>` -> delete (honours `?resourceVersion=` CAS)
//!  - `PATCH  /<collection>/<name>` -> strategic-merge / merge / json-patch
//!    or server-side apply when Content-Type: application/apply-patch+yaml

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use crate::apply;
use crate::error::{storage_error, ApiError};
use crate::state::{
    item_key, resolve, resource_revision, set_resource_version, set_type_meta,
    AppState, Loc,
};

/// `DELETE /<item>?resourceVersion=&dryRun=`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct DeleteParams {
    #[serde(default, rename = "resourceVersion")]
    pub resource_version: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared logic (do_*)
// ---------------------------------------------------------------------------

pub(crate) async fn do_get(st: &AppState, loc: &Loc, name: &str) -> Response {
    let res = match resolve(&st.registry, &loc.group, &loc.version, &loc.resource) {
        Some(r) => r,
        None => return ApiError::NotFoundResource.into_response(),
    };
    if res.scope.is_namespaced() != loc.namespace.is_some() {
        return ApiError::NotFoundResource.into_response();
    }
    let key = item_key(loc, &res, name);
    match st.store.get(&key).await {
        Ok(Some(entry)) => {
            let mut v = entry.value;
            set_resource_version(&mut v, entry.mod_revision);
            (StatusCode::OK, Json(v)).into_response()
        }
        Ok(None) => ApiError::NotFound { kind: res.kind, name: name.to_string() }.into_response(),
        Err(e) => storage_error(e, &res.kind, name).into_response(),
    }
}

pub(crate) async fn do_replace(st: &AppState, loc: &Loc, name: &str, mut body: Value) -> Response {
    let res = match resolve(&st.registry, &loc.group, &loc.version, &loc.resource) {
        Some(r) => r,
        None => return ApiError::NotFoundResource.into_response(),
    };
    if res.scope.is_namespaced() != loc.namespace.is_some() {
        return ApiError::NotFoundResource.into_response();
    }
    let key = item_key(loc, &res, name);
    let if_revision = resource_revision(&body);
    set_type_meta(&mut body, &res.api_version, &res.kind);
    match st.store.update(&key, body, if_revision).await {
        Ok(entry) => {
            let mut v = entry.value;
            set_resource_version(&mut v, entry.mod_revision);
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(e) => storage_error(e, &res.kind, name).into_response(),
    }
}

pub(crate) async fn do_delete(st: &AppState, loc: &Loc, name: &str, params: &DeleteParams) -> Response {
    let res = match resolve(&st.registry, &loc.group, &loc.version, &loc.resource) {
        Some(r) => r,
        None => return ApiError::NotFoundResource.into_response(),
    };
    if res.scope.is_namespaced() != loc.namespace.is_some() {
        return ApiError::NotFoundResource.into_response();
    }
    let key = item_key(loc, &res, name);
    let if_revision = params.resource_version.as_deref().and_then(|s| s.parse::<u64>().ok());
    match st.store.delete(&key, if_revision).await {
        Ok(Some(entry)) => {
            let mut v = entry.value;
            set_resource_version(&mut v, entry.mod_revision);
            set_type_meta(&mut v, &res.api_version, &res.kind);
            (StatusCode::OK, Json(v)).into_response()
        }
        Ok(None) => ApiError::NotFound { kind: res.kind, name: name.to_string() }.into_response(),
        Err(e) => storage_error(e, &res.kind, name).into_response(),
    }
}

pub(crate) async fn do_patch(
    st: &AppState,
    loc: &Loc,
    name: &str,
    content_type: &str,
    patch: &[u8],
) -> Response {
    let res = match resolve(&st.registry, &loc.group, &loc.version, &loc.resource) {
        Some(r) => r,
        None => return ApiError::NotFoundResource.into_response(),
    };
    if res.scope.is_namespaced() != loc.namespace.is_some() {
        return ApiError::NotFoundResource.into_response();
    }
    let key = item_key(loc, &res, name);
    let current = match st.store.get(&key).await {
        Ok(Some(e)) => e,
        Ok(None) => {
            return ApiError::NotFound { kind: res.kind, name: name.to_string() }.into_response()
        }
        Err(e) => return storage_error(e, &res.kind, name).into_response(),
    };
    let if_revision = Some(current.mod_revision);
    let mut merged = current.value.clone();
    if let Err(e) = apply_patch(&mut merged, content_type, patch) {
        return e.into_response();
    }
    set_type_meta(&mut merged, &res.api_version, &res.kind);
    match st.store.update(&key, merged, if_revision).await {
        Ok(entry) => {
            let mut v = entry.value;
            set_resource_version(&mut v, entry.mod_revision);
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(e) => storage_error(e, &res.kind, name).into_response(),
    }
}

/// Apply a patch per its Content-Type:
///  - `application/strategic-merge-patch+json` -> core/v1 strategies
///  - `application/merge-patch+json`           -> RFC 7386 (deep merge)
///  - `application/json-patch+json`            -> RFC 6902
///
/// Unknown content-type falls back to strategic merge (kubectl default).
fn apply_patch(target: &mut Value, content_type: &str, patch: &[u8]) -> Result<(), ApiError> {
    if content_type.contains("json-patch") {
        let p: api::patch::JsonPatch =
            serde_json::from_slice(patch).map_err(|e| ApiError::BadRequest(e.to_string()))?;
        api::patch::apply_json_patch(target, &p)
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        return Ok(());
    }
    let patch_value: Value =
        serde_json::from_slice(patch).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let strategy = if content_type.contains("merge-patch") && !content_type.contains("strategic") {
        api::patch::PatchStrategy::default()
    } else {
        api::patch::PatchStrategy::kubernetes_defaults()
    };
    api::patch::strategic_merge(target, &patch_value, &strategy)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(())
}

/// Parse a JSON body or return a 400 response.
fn parse_json(body: &[u8]) -> Option<Value> {
    serde_json::from_slice(body).ok()
}

/// Extract the Content-Type header (defaults to `application/json`).
fn ct_default_json(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string()
}

// ---------------------------------------------------------------------------
// axum wrappers (core `""` group + grouped groups)
// ---------------------------------------------------------------------------

// --- core group items ---

pub(crate) async fn core_get(
    State(st): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
) -> Response {
    do_get(&st, &Loc::new("", "v1", resource, None), &name).await
}

pub(crate) async fn core_replace(
    State(st): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    headers: HeaderMap,
    Query(q): Query<apply::ApplyQuery>,
    body: Bytes,
) -> Response {
    let ct = ct_default_json(&headers);
    if apply::is_apply_ct(&ct) {
        let desired = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
        return apply::do_apply(&st, &Loc::new("", "v1", resource, None), &name, desired, &q).await;
    }
    let body = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
    do_replace(&st, &Loc::new("", "v1", resource, None), &name, body).await
}

pub(crate) async fn core_delete(
    State(st): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    Query(q): Query<DeleteParams>,
) -> Response {
    do_delete(&st, &Loc::new("", "v1", resource, None), &name, &q).await
}

pub(crate) async fn core_patch(
    State(st): State<AppState>,
    Path((resource, name)): Path<(String, String)>,
    headers: HeaderMap,
    Query(q): Query<apply::ApplyQuery>,
    body: Bytes,
) -> Response {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/strategic-merge-patch+json");
    if apply::is_apply_ct(ct) {
        let desired = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
        return apply::do_apply(&st, &Loc::new("", "v1", resource, None), &name, desired, &q).await;
    }
    do_patch(&st, &Loc::new("", "v1", resource, None), &name, ct, &body).await
}

// --- core group, namespaced items ---

pub(crate) async fn core_get_ns(
    State(st): State<AppState>,
    Path((ns, resource, name)): Path<(String, String, String)>,
) -> Response {
    do_get(&st, &Loc::new("", "v1", resource, Some(ns)), &name).await
}

pub(crate) async fn core_replace_ns(
    State(st): State<AppState>,
    Path((ns, resource, name)): Path<(String, String, String)>,
    headers: HeaderMap,
    Query(q): Query<apply::ApplyQuery>,
    body: Bytes,
) -> Response {
    let ct = ct_default_json(&headers);
    if apply::is_apply_ct(&ct) {
        let desired = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
        return apply::do_apply(&st, &Loc::new("", "v1", resource, Some(ns)), &name, desired, &q).await;
    }
    let body = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
    do_replace(&st, &Loc::new("", "v1", resource, Some(ns)), &name, body).await
}

pub(crate) async fn core_delete_ns(
    State(st): State<AppState>,
    Path((ns, resource, name)): Path<(String, String, String)>,
    Query(q): Query<DeleteParams>,
) -> Response {
    do_delete(&st, &Loc::new("", "v1", resource, Some(ns)), &name, &q).await
}

pub(crate) async fn core_patch_ns(
    State(st): State<AppState>,
    Path((ns, resource, name)): Path<(String, String, String)>,
    headers: HeaderMap,
    Query(q): Query<apply::ApplyQuery>,
    body: Bytes,
) -> Response {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/strategic-merge-patch+json");
    if apply::is_apply_ct(ct) {
        let desired = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
        return apply::do_apply(&st, &Loc::new("", "v1", resource, Some(ns)), &name, desired, &q).await;
    }
    do_patch(&st, &Loc::new("", "v1", resource, Some(ns)), &name, ct, &body).await
}

// --- grouped items ---

pub(crate) async fn grp_get(
    State(st): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
) -> Response {
    do_get(&st, &Loc::new(&group, &version, resource, None), &name).await
}

pub(crate) async fn grp_replace(
    State(st): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    Query(q): Query<apply::ApplyQuery>,
    body: Bytes,
) -> Response {
    let ct = ct_default_json(&headers);
    if apply::is_apply_ct(&ct) {
        let desired = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
        return apply::do_apply(&st, &Loc::new(&group, &version, resource, None), &name, desired, &q).await;
    }
    let body = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
    do_replace(&st, &Loc::new(&group, &version, resource, None), &name, body).await
}

pub(crate) async fn grp_delete(
    State(st): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    Query(q): Query<DeleteParams>,
) -> Response {
    do_delete(&st, &Loc::new(&group, &version, resource, None), &name, &q).await
}

pub(crate) async fn grp_patch(
    State(st): State<AppState>,
    Path((group, version, resource, name)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    Query(q): Query<apply::ApplyQuery>,
    body: Bytes,
) -> Response {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/strategic-merge-patch+json");
    if apply::is_apply_ct(ct) {
        let desired = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
        return apply::do_apply(&st, &Loc::new(&group, &version, resource, None), &name, desired, &q).await;
    }
    do_patch(&st, &Loc::new(&group, &version, resource, None), &name, ct, &body).await
}

// --- grouped, namespaced items ---

pub(crate) async fn grp_get_ns(
    State(st): State<AppState>,
    Path((group, version, ns, resource, name)): Path<(String, String, String, String, String)>,
) -> Response {
    do_get(&st, &Loc::new(&group, &version, resource, Some(ns)), &name).await
}

pub(crate) async fn grp_replace_ns(
    State(st): State<AppState>,
    Path((group, version, ns, resource, name)): Path<(String, String, String, String, String)>,
    headers: HeaderMap,
    Query(q): Query<apply::ApplyQuery>,
    body: Bytes,
) -> Response {
    let ct = ct_default_json(&headers);
    if apply::is_apply_ct(&ct) {
        let desired = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
        return apply::do_apply(&st, &Loc::new(&group, &version, resource, Some(ns)), &name, desired, &q).await;
    }
    let body = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
    do_replace(&st, &Loc::new(&group, &version, resource, Some(ns)), &name, body).await
}

pub(crate) async fn grp_delete_ns(
    State(st): State<AppState>,
    Path((group, version, ns, resource, name)): Path<(String, String, String, String, String)>,
    Query(q): Query<DeleteParams>,
) -> Response {
    do_delete(&st, &Loc::new(&group, &version, resource, Some(ns)), &name, &q).await
}

pub(crate) async fn grp_patch_ns(
    State(st): State<AppState>,
    Path((group, version, ns, resource, name)): Path<(String, String, String, String, String)>,
    headers: HeaderMap,
    Query(q): Query<apply::ApplyQuery>,
    body: Bytes,
) -> Response {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/strategic-merge-patch+json");
    if apply::is_apply_ct(ct) {
        let desired = match parse_json(&body) { Some(v) => v, None => return ApiError::BadRequest("invalid JSON".into()).into_response() };
        return apply::do_apply(&st, &Loc::new(&group, &version, resource, Some(ns)), &name, desired, &q).await;
    }
    do_patch(&st, &Loc::new(&group, &version, resource, Some(ns)), &name, ct, &body).await
}
