//! Node status subresource (workstream A; upstream `nodes/status`).
//!
//! `PUT|PATCH /api/v1/nodes/<name>/status` — the status write path a
//! kubelet-equivalent drives for heartbeats/capacity. Cluster-scoped: no
//! namespace segment. Only the `.status` subtree is writable through this
//! route — `metadata`/`spec` are preserved untouched (same contract as the
//! pod status route, T4.2).
//!
//! Semantics (upstream parity):
//!  - node missing -> `404 nodes "<name>" not found`
//!  - PUT body without a `.status` field -> `422 Invalid`
//!  - concurrent writer between the read and the CAS -> `409 Conflict`.
//!
//! PATCH honours the standard content types (strategic-merge default,
//! RFC 7386 merge, RFC 6902 json-patch; server-side-apply CTs ride the
//! main-resource apply path) and writes back only the patched `.status`.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::apply::ApplyQuery;
use crate::error::{storage_error, ApiError};
use crate::item;
use crate::state::Loc;
use crate::state::{cluster_key, set_resource_version, AppState};

/// `PUT /api/v1/nodes/{name}/status`
pub(crate) async fn put_node_status(
    State(st): State<AppState>,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let parsed = match serde_json::from_slice::<Value>(&body) {
        Ok(v) => v,
        Err(_) => return ApiError::BadRequest("invalid JSON".into()).into_response(),
    };

    let key = cluster_key("", "nodes", &name);
    let entry = match st.store.get(&key).await {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return ApiError::NotFound {
                kind: "nodes".into(),
                name,
            }
            .into_response()
        }
        Err(e) => return storage_error(e, "nodes", &name).into_response(),
    };

    // Only `.status` is writable here; an empty/null status still overwrites
    // (a kubelet may clear it), but a body without the field at all is
    // invalid — same rule as `put_pod_status`.
    let Some(new_status) = parsed.pointer("/status").cloned() else {
        return ApiError::Invalid {
            kind: "Node".into(),
            message: "body must carry a .status field".into(),
        }
        .into_response();
    };

    // CAS on the revision just read, NOT the embedded resourceVersion:
    // written values are stored verbatim, so a stale embedded RV would 409
    // forever after any client read-modify-write (T4.2 lesson).
    let cas_rev = Some(entry.mod_revision);
    let mut node = entry.value;
    node["status"] = new_status;
    match st.store.update(&key, node, cas_rev).await {
        Ok(mut entry) => {
            set_resource_version(&mut entry.value, entry.mod_revision);
            (StatusCode::OK, Json(entry.value)).into_response()
        }
        Err(e) => storage_error(e, "nodes", &name).into_response(),
    }
}

/// `PATCH /api/v1/nodes/{name}/status`
///
/// The patch is applied to the live object, but only the resulting
/// `.status` is persisted — spec/metadata edits through this route are
/// dropped (upstream status-writability parity).
pub(crate) async fn patch_node_status(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(q): Query<ApplyQuery>,
    body: Bytes,
) -> Response {
    // Mirror `core_patch`: server-side apply rides the main-resource path.
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/strategic-merge-patch+json");
    if crate::apply::is_apply_ct(ct) {
        let desired = match serde_json::from_slice::<Value>(&body) {
            Ok(v) => v,
            Err(_) => return ApiError::BadRequest("invalid JSON".into()).into_response(),
        };
        let loc = Loc::new("", "v1", "nodes".into(), None);
        return crate::apply::do_apply(&st, &loc, &name, desired, &q).await;
    }

    let key = cluster_key("", "nodes", &name);
    let entry = match st.store.get(&key).await {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return ApiError::NotFound {
                kind: "nodes".into(),
                name,
            }
            .into_response()
        }
        Err(e) => return storage_error(e, "nodes", &name).into_response(),
    };

    // Patch a scratch copy; persist only the `.status` subtree it produced.
    let mut merged = entry.value.clone();
    if let Err(e) = item::apply_patch(&mut merged, ct, &body) {
        return e.into_response();
    }
    let cas_rev = Some(entry.mod_revision);
    let mut node = entry.value;
    if let Some(new_status) = merged.get("status").cloned() {
        node["status"] = new_status;
    }
    match st.store.update(&key, node, cas_rev).await {
        Ok(mut entry) => {
            set_resource_version(&mut entry.value, entry.mod_revision);
            (StatusCode::OK, Json(entry.value)).into_response()
        }
        Err(e) => storage_error(e, "nodes", &name).into_response(),
    }
}
