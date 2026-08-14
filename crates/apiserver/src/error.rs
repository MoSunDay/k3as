//! Kubernetes `Status` error mapping (TODO **T1.2b** + **T1.2c**).
//!
//! Converts storage-layer and request-shape errors into the canonical
//! `metav1.Status` JSON body with the right HTTP code + `reason`, so kubectl
//! and kube-rs clients recognise conflicts / not-found exactly as upstream.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use storage::StorageError;

use crate::state::NOT_FOUND_RESOURCE_MSG;

/// API-layer error: maps cleanly to a `Status` body.
pub(crate) enum ApiError {
    /// `GET`/`POST`/`PUT`/`DELETE` on an unknown or unserved resource, or a
    /// scope/path mismatch (e.g. a namespaced object via a cluster-scoped path).
    NotFoundResource,
    /// `GET`/`DELETE` on a known resource whose named object does not exist.
    NotFound { kind: String, name: String },
    /// `POST` create collided with an existing key.
    AlreadyExists { kind: String, name: String },
    /// `PUT`/`DELETE` failed the `resourceVersion` CAS check.
    Conflict { kind: String, name: String },
    /// Server-side apply conflict: another field manager owns a field.
    ApplyConflict {
        kind: String,
        name: String,
        /// `(path, manager)` pairs.
        conflicts: Vec<(String, String)>,
    },
    /// A watch `resourceVersion` at or below the storage compaction
    /// watermark: the requested history is gone (upstream `410 Gone`,
    /// reason `Expired`).
    Gone { requested: u64, watermark: u64 },
    /// The request body was missing required fields (e.g. `metadata.name`).
    Invalid { kind: String, message: String },
    /// Malformed JSON or unsupported content type.
    BadRequest(String),
    /// Anything else.
    Internal(String),
}

impl ApiError {
    /// `(code, reason, message, details)` for the Status body.
    fn fields(&self) -> (StatusCode, &'static str, String, Value) {
        match self {
            ApiError::NotFoundResource => (
                StatusCode::NOT_FOUND,
                "NotFound",
                NOT_FOUND_RESOURCE_MSG.to_string(),
                Value::Null,
            ),
            ApiError::NotFound { kind, name } => (
                StatusCode::NOT_FOUND,
                "NotFound",
                format!("{kind} \"{name}\" not found"),
                json!({ "kind": kind, "name": name }),
            ),
            ApiError::AlreadyExists { kind, name } => (
                StatusCode::CONFLICT,
                "AlreadyExists",
                format!("{kind}s \"{name}\" already exists"),
                json!({ "kind": kind, "name": name }),
            ),
            ApiError::Conflict { kind, name } => (
                StatusCode::CONFLICT,
                "Conflict",
                format!(
                    "Operation cannot be fulfilled on {kind} \"{name}\": the object has been \
                     modified; please apply your changes to the latest version and try again"
                ),
                json!({ "kind": kind, "name": name }),
            ),
            ApiError::ApplyConflict {
                kind,
                name,
                conflicts,
            } => {
                let causes: Vec<Value> = conflicts
                    .iter()
                    .map(|(path, mgr)| {
                        json!({
                            "reason": "FieldManagerConflict",
                            "message": format!("conflict: \"{mgr}\" owns \"{path}\""),
                            "field": path,
                        })
                    })
                    .collect();
                let n = conflicts.len();
                let detail = conflicts
                    .iter()
                    .map(|(p, m)| format!("\"{m}\" owns \"{p}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let plural = if n == 1 { "" } else { "s" };
                (
                    StatusCode::CONFLICT,
                    "Conflict",
                    format!("Apply failed with {n} conflict{plural}: {detail}"),
                    json!({ "kind": kind, "name": name, "causes": causes }),
                )
            }
            ApiError::Gone {
                requested,
                watermark,
            } => (
                StatusCode::GONE,
                "Expired",
                format!("too old resource version: {requested} ({watermark})"),
                Value::Null,
            ),
            ApiError::Invalid { kind, message } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid",
                format!("{kind} \"{message}\""),
                json!({ "kind": kind }),
            ),
            ApiError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                "BadRequest",
                msg.clone(),
                Value::Null,
            ),
            ApiError::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                msg.clone(),
                Value::Null,
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, reason, message, details) = self.fields();
        let mut body = json!({
            "kind": "Status",
            "apiVersion": "v1",
            "metadata": {},
            "status": "Failure",
            "message": message,
            "reason": reason,
            "code": code.as_u16(),
        });
        if !details.is_null() {
            body["details"] = details;
        }
        (code, Json(body)).into_response()
    }
}

/// Map a [`StorageError`] to an [`ApiError`] for a given kind/name.
pub(crate) fn storage_error(err: StorageError, kind: &str, name: &str) -> ApiError {
    match err {
        StorageError::NotFound { .. } => ApiError::NotFound {
            kind: kind.to_string(),
            name: name.to_string(),
        },
        StorageError::AlreadyExists { .. } => ApiError::AlreadyExists {
            kind: kind.to_string(),
            name: name.to_string(),
        },
        StorageError::Conflict {
            expected: _,
            have: _,
            ..
        } => ApiError::Conflict {
            kind: kind.to_string(),
            name: name.to_string(),
        },
        StorageError::Compacted {
            requested,
            watermark,
        } => ApiError::Gone {
            requested,
            watermark,
        },
        StorageError::InvalidKey { key } => {
            ApiError::BadRequest(format!("invalid storage key: {key}"))
        }
        other => ApiError::Internal(other.to_string()),
    }
}
