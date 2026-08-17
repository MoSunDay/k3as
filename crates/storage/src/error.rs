//! Storage errors (T2.2; backend-agnostic failure variant added in T2.3).

use thiserror::Error;

use crate::entry::Revision;

/// Errors from the storage layer.
#[derive(Debug, Error)]
pub enum StorageError {
    /// A create collided with an existing key.
    #[error("resource already exists: {key}")]
    AlreadyExists { key: String },
    /// A get/update/delete targeted a missing key.
    #[error("resource not found: {key}")]
    NotFound { key: String },
    /// An optimistic-concurrency (CAS) check failed: the live `mod_revision`
    /// did not match the caller's `if_revision`.
    #[error("optimistic concurrency conflict on {key}: expected {expected:?}, have {have:?}")]
    Conflict {
        key: String,
        expected: Option<Revision>,
        have: Option<Revision>,
    },
    /// A watch requested a start revision at or below the history-compaction
    /// watermark (etcd `ErrCompacted`; surfaces upstream as watch `410 Gone`).
    #[error("requested revision {requested} is compacted (watermark {watermark})")]
    Compacted {
        requested: Revision,
        watermark: Revision,
    },
    #[error("invalid storage key: {key}")]
    InvalidKey { key: String },
    /// A backend-specific failure (SQLite/libsql IO or SQL error, T2.3).
    #[error("storage backend error: {0}")]
    Backend(String),
    /// The backend (or its watch channel) has been closed.
    #[error("storage backend closed")]
    Closed,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
