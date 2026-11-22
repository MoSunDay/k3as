//! axum wrapper handlers for the per-item REST routes (TODO **T1.2b** +
//! **T1.2c**; split out of `item.rs` in **S52-D0** to respect the file-size
//! cap). Thin shells: they parse the wire body / Content-Type and delegate
//! to the shared `do_*` logic in [`crate::item`] (which carries the Service
//! write-path admission, S52-D0 / Q44, for replace and patch).
//!
//! Routes (core group `""` + grouped):
//!  - `GET    /<collection>/<name>` -> read one object
//!  - `PUT    /<collection>/<name>` -> replace (honours `metadata.resourceVersion` CAS)
//!    or server-side apply when Content-Type: application/apply-patch+yaml
//!  - `DELETE /<collection>/<name>` -> delete (honours `?resourceVersion=` CAS);
//!    finalizer-gated soft delete + `propagationPolicy=Orphan` (T3.1b, Q20)
//!  - `PATCH  /<collection>/<name>` -> strategic-merge / merge / json-patch
//!    or server-side apply when Content-Type: application/apply-patch+yaml

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::apply;
use crate::error::ApiError;
use crate::item::{do_delete, do_get, do_patch, do_replace, DeleteParams};
use crate::state::{AppState, Loc};

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
        let desired = match parse_json(&body) {
            Some(v) => v,
            None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
        };
        return apply::do_apply(&st, &Loc::new("", "v1", resource, None), &name, desired, &q).await;
    }
    let body = match parse_json(&body) {
        Some(v) => v,
        None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
    };
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
        let desired = match parse_json(&body) {
            Some(v) => v,
            None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
        };
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
        let desired = match parse_json(&body) {
            Some(v) => v,
            None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
        };
        return apply::do_apply(
            &st,
            &Loc::new("", "v1", resource, Some(ns)),
            &name,
            desired,
            &q,
        )
        .await;
    }
    let body = match parse_json(&body) {
        Some(v) => v,
        None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
    };
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
        let desired = match parse_json(&body) {
            Some(v) => v,
            None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
        };
        return apply::do_apply(
            &st,
            &Loc::new("", "v1", resource, Some(ns)),
            &name,
            desired,
            &q,
        )
        .await;
    }
    do_patch(
        &st,
        &Loc::new("", "v1", resource, Some(ns)),
        &name,
        ct,
        &body,
    )
    .await
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
        let desired = match parse_json(&body) {
            Some(v) => v,
            None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
        };
        return apply::do_apply(
            &st,
            &Loc::new(&group, &version, resource, None),
            &name,
            desired,
            &q,
        )
        .await;
    }
    let body = match parse_json(&body) {
        Some(v) => v,
        None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
    };
    do_replace(
        &st,
        &Loc::new(&group, &version, resource, None),
        &name,
        body,
    )
    .await
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
        let desired = match parse_json(&body) {
            Some(v) => v,
            None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
        };
        return apply::do_apply(
            &st,
            &Loc::new(&group, &version, resource, None),
            &name,
            desired,
            &q,
        )
        .await;
    }
    do_patch(
        &st,
        &Loc::new(&group, &version, resource, None),
        &name,
        ct,
        &body,
    )
    .await
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
        let desired = match parse_json(&body) {
            Some(v) => v,
            None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
        };
        return apply::do_apply(
            &st,
            &Loc::new(&group, &version, resource, Some(ns)),
            &name,
            desired,
            &q,
        )
        .await;
    }
    let body = match parse_json(&body) {
        Some(v) => v,
        None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
    };
    do_replace(
        &st,
        &Loc::new(&group, &version, resource, Some(ns)),
        &name,
        body,
    )
    .await
}

pub(crate) async fn grp_delete_ns(
    State(st): State<AppState>,
    Path((group, version, ns, resource, name)): Path<(String, String, String, String, String)>,
    Query(q): Query<DeleteParams>,
) -> Response {
    do_delete(
        &st,
        &Loc::new(&group, &version, resource, Some(ns)),
        &name,
        &q,
    )
    .await
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
        let desired = match parse_json(&body) {
            Some(v) => v,
            None => return ApiError::BadRequest("invalid JSON".into()).into_response(),
        };
        return apply::do_apply(
            &st,
            &Loc::new(&group, &version, resource, Some(ns)),
            &name,
            desired,
            &q,
        )
        .await;
    }
    do_patch(
        &st,
        &Loc::new(&group, &version, resource, Some(ns)),
        &name,
        ct,
        &body,
    )
    .await
}
