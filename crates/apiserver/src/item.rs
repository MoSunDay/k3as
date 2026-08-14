//! REST per-item handlers: get / replace / delete / patch / apply (T1.2b + T1.2c).
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
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::apply;
use crate::error::{storage_error, ApiError};
use crate::state::{
    item_key, resolve, resource_revision, set_resource_version, set_type_meta, AppState, Loc,
};

/// `DELETE /<item>?resourceVersion=&propagationPolicy=&dryRun=`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct DeleteParams {
    #[serde(default, rename = "resourceVersion")]
    pub resource_version: Option<String>,
    /// Deletion propagation policy (T3.1b, Q20): `Orphan` strips
    /// ownerReferences instead of cascading; `Background` (and `Foreground`,
    /// treated as Background in v1) is the default hard delete.
    #[serde(default, rename = "propagationPolicy")]
    pub propagation_policy: Option<String>,
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
        Ok(None) => ApiError::NotFound {
            kind: res.kind,
            name: name.to_string(),
        }
        .into_response(),
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
            finalize_if_complete(st, &key, &entry).await;
            let mut v = entry.value;
            set_resource_version(&mut v, entry.mod_revision);
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(e) => storage_error(e, &res.kind, name).into_response(),
    }
}

/// Finalizer-gated DELETE (TODO **T3.1b**, decision **Q20**):
///
/// 1. read-first: the live body decides the delete mode (absent -> `404`);
/// 2. `metadata.finalizers` non-empty -> soft delete: stamp
///    `metadata.deletionTimestamp` (idempotent: already-stamped objects are
///    returned unchanged); the object stays until a writer empties the
///    finalizers (see [`finalize_if_complete`]);
/// 3. `propagationPolicy=Orphan` -> CAS-update the
///    `init-pro.io/deletion-propagation` annotation in, then hard delete so
///    the emitted DELETE event carries the marker the GC reads;
/// 4. otherwise -> hard delete (`Background`; `Foreground` is simplified to
///    `Background` in v1 -- foreground finalizers are not modeled, Q20).
///
/// For finalizer-less objects the wire behavior is byte-identical to the
/// pre-T3.1b path (golden G10/G11).
pub(crate) async fn do_delete(
    st: &AppState,
    loc: &Loc,
    name: &str,
    params: &DeleteParams,
) -> Response {
    let res = match resolve(&st.registry, &loc.group, &loc.version, &loc.resource) {
        Some(r) => r,
        None => return ApiError::NotFoundResource.into_response(),
    };
    if res.scope.is_namespaced() != loc.namespace.is_some() {
        return ApiError::NotFoundResource.into_response();
    }
    let key = item_key(loc, &res, name);
    let cas_revision = params
        .resource_version
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok());
    let current = match st.store.get(&key).await {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return ApiError::NotFound {
                kind: res.kind,
                name: name.to_string(),
            }
            .into_response()
        }
        Err(e) => return storage_error(e, &res.kind, name).into_response(),
    };
    if has_finalizers(&current.value) {
        return soft_delete(st, &res, &key, current, cas_revision, name).await;
    }
    if params.propagation_policy.as_deref() == Some("Orphan") {
        return orphan_delete(st, &res, &key, current, name).await;
    }
    match st.store.delete(&key, cas_revision).await {
        Ok(Some(entry)) => OkDeleted {
            value: entry.value,
            mod_revision: entry.mod_revision,
            api_version: &res.api_version,
            kind: &res.kind,
        }
        .response(),
        Ok(None) => ApiError::NotFound {
            kind: res.kind,
            name: name.to_string(),
        }
        .into_response(),
        Err(e) => storage_error(e, &res.kind, name).into_response(),
    }
}

/// Shorthand for the hard-delete 200 body: the removed object with fresh
/// `resourceVersion` + type meta projected (Q10 wire shape).
struct OkDeleted<'a> {
    value: Value,
    mod_revision: u64,
    api_version: &'a str,
    kind: &'a str,
}

impl OkDeleted<'_> {
    fn response(self) -> Response {
        let mut v = self.value;
        set_resource_version(&mut v, self.mod_revision);
        set_type_meta(&mut v, self.api_version, self.kind);
        (StatusCode::OK, Json(v)).into_response()
    }
}

/// Soft delete: stamp `deletionTimestamp` (first DELETE only; repeat DELETEs
/// return the stored body unchanged -- idempotent, upstream parity).
async fn soft_delete(
    st: &AppState,
    res: &crate::state::Resolved,
    key: &storage::Key,
    current: storage::StoredEntry,
    cas_revision: Option<u64>,
    name: &str,
) -> Response {
    if current
        .value
        .pointer("/metadata/deletionTimestamp")
        .is_some()
    {
        return OkDeleted {
            value: current.value,
            mod_revision: current.mod_revision,
            api_version: &res.api_version,
            kind: &res.kind,
        }
        .response();
    }
    let mut body = current.value;
    set_deletion_timestamp(&mut body, &deletion_now());
    // Prefer the request's resourceVersion CAS when present, else pin the
    // revision we just read (both lose the race identically -> 409).
    let if_revision = cas_revision.or(Some(current.mod_revision));
    match st.store.update(key, body, if_revision).await {
        Ok(entry) => OkDeleted {
            value: entry.value,
            mod_revision: entry.mod_revision,
            api_version: &res.api_version,
            kind: &res.kind,
        }
        .response(),
        Err(e) => storage_error(e, &res.kind, name).into_response(),
    }
}

/// Orphan delete: write the propagation marker, then hard delete with the
/// fresh revision so the DELETE event (what the GC consumes) carries it.
async fn orphan_delete(
    st: &AppState,
    res: &crate::state::Resolved,
    key: &storage::Key,
    current: storage::StoredEntry,
    name: &str,
) -> Response {
    let mut body = current.value;
    merge_annotation(&mut body, "init-pro.io/deletion-propagation", "Orphan");
    match st.store.update(key, body, Some(current.mod_revision)).await {
        Ok(entry) => match st.store.delete(key, Some(entry.mod_revision)).await {
            Ok(Some(deleted)) => OkDeleted {
                value: deleted.value,
                mod_revision: deleted.mod_revision,
                api_version: &res.api_version,
                kind: &res.kind,
            }
            .response(),
            Ok(None) => ApiError::NotFound {
                kind: res.kind.clone(),
                name: name.to_string(),
            }
            .into_response(),
            Err(e) => storage_error(e, &res.kind, name).into_response(),
        },
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
            return ApiError::NotFound {
                kind: res.kind,
                name: name.to_string(),
            }
            .into_response()
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
            finalize_if_complete(st, &key, &entry).await;
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

// ---------------------------------------------------------------------------
// Finalizer helpers (T3.1b, Q20)
// ---------------------------------------------------------------------------

/// True when `metadata.finalizers` is a non-empty array (delete is gated).
fn has_finalizers(v: &Value) -> bool {
    v.pointer("/metadata/finalizers")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

/// True when the stored body completed a soft delete: `deletionTimestamp`
/// set AND finalizers empty/absent (upstream: the object must be removed).
fn deletion_finalized(v: &Value) -> bool {
    v.pointer("/metadata/deletionTimestamp").is_some() && !has_finalizers(v)
}

/// Best-effort terminal delete after a successful PUT/PATCH: an object whose
/// finalizers emptied while a `deletionTimestamp` was set is removed
/// (upstream apiserver behavior). In-process controllers bypass the
/// apiserver (Q19) -- the namespace controller performs its own terminal
/// delete; this rule covers kubectl-driven PUT/PATCH flows.
async fn finalize_if_complete(st: &AppState, key: &storage::Key, entry: &storage::StoredEntry) {
    if !deletion_finalized(&entry.value) {
        return;
    }
    if let Err(e) = st.store.delete(key, None).await {
        // A concurrent delete winning the race is fine; anything else is
        // surfaced but must not fail the just-successful write.
        tracing::debug!(key = %key.as_path(), error = %e, "finalizer-completion delete failed");
    }
}

/// Set `metadata.deletionTimestamp` (soft-delete stamp).
fn set_deletion_timestamp(v: &mut Value, ts: &str) {
    if let Some(meta) = v.get_mut("metadata").and_then(Value::as_object_mut) {
        meta.insert("deletionTimestamp".into(), Value::String(ts.to_string()));
    }
}

/// Merge one entry into `metadata.annotations` (existing map preserved).
fn merge_annotation(v: &mut Value, key: &str, val: &str) {
    let Some(obj) = v.as_object_mut() else { return };
    let meta = obj
        .entry("metadata")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let annotations = meta
        .as_object_mut()
        .map(|m| {
            m.entry("annotations")
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
        })
        .and_then(Value::as_object_mut);
    if let Some(a) = annotations {
        a.insert(key.to_string(), Value::String(val.to_string()));
    }
}

/// Current wall clock in RFC3339 (`common::time`, shared with controllers).
fn deletion_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    common::time::now_rfc3339(secs)
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
