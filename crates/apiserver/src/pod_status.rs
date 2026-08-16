//! Pod status subresource (TODO **T4.2**).
//!
//! `PUT /api/v1/namespaces/<ns>/pods/<name>/status` — the upstream status
//! write path the kubelet drives (`pods/status`). By upstream convention the
//! kubelet PUTs the **full Pod**, but only `.status` is writable through this
//! route: the stored object's `metadata`/`spec` are preserved untouched and
//! only the `.status` subtree is replaced.
//!
//! Semantics (upstream parity):
//!  - pod missing -> `404 pods "<name>" not found`
//!  - body without a `.status` field -> `422 Invalid`
//!  - concurrent writer between the read and the CAS -> `409 Conflict`.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::error::{storage_error, ApiError};
use crate::state::{namespaced_key, set_resource_version, AppState};

/// `PUT /api/v1/namespaces/{namespace}/pods/{name}/status`
pub(crate) async fn put_pod_status(
    State(st): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let parsed = match serde_json::from_slice::<Value>(&body) {
        Ok(v) => v,
        Err(_) => return ApiError::BadRequest("invalid JSON".into()).into_response(),
    };

    let key = namespaced_key("", "pods", &namespace, &name);
    let entry = match st.store.get(&key).await {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return ApiError::NotFound {
                kind: "pods".into(),
                name,
            }
            .into_response()
        }
        Err(e) => return storage_error(e, "pods", &name).into_response(),
    };

    // Only `.status` is writable here; an empty/null status still overwrites
    // (kubelet may clear it), but a body without the field at all is invalid.
    let Some(new_status) = parsed.pointer("/status").cloned() else {
        return ApiError::Invalid {
            kind: "Pod".into(),
            message: "body must carry a .status field".into(),
        }
        .into_response();
    };

    // CAS on the revision just read (the entry's mod_revision). NOT the
    // embedded metadata.resourceVersion: written values are stored verbatim,
    // so after any client read-modify-write (e.g. the scheduler binding) the
    // embedded RV lags mod_revision and a CAS on it 409s forever (seen live:
    // kubelet status writes looping 409, pods stuck non-Ready, T4.2).
    let cas_rev = Some(entry.mod_revision);
    let mut pod = entry.value;
    pod["status"] = new_status;
    match st.store.update(&key, pod, cas_rev).await {
        Ok(mut entry) => {
            set_resource_version(&mut entry.value, entry.mod_revision);
            (StatusCode::OK, Json(entry.value)).into_response()
        }
        Err(e) => storage_error(e, "pods", &name).into_response(),
    }
}
