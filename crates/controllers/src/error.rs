//! Controller error surface (T3.1a).
//!
//! Wraps [`StorageError`] plus controller-specific signals. CAS races surface
//! as `Conflict` so callers can treat them as a benign "retry via requeue"
//! rather than a hard failure.

use thiserror::Error;

use storage::StorageError;

/// Errors raised by the controller framework and reconcilers.
#[derive(Debug, Error)]
pub enum ControllerError {
    /// Underlying storage failure.
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    /// Optimistic-concurrency race; retry via the normal requeue path.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Unexpected internal shape/state.
    #[error("internal: {0}")]
    Internal(String),
}

impl ControllerError {
    /// True when the error is a CAS race (retry-able, not a failure).
    pub fn is_conflict(&self) -> bool {
        matches!(
            self,
            ControllerError::Conflict(_) | ControllerError::Storage(StorageError::Conflict { .. })
        )
    }

    /// True when the target already existed (benign; controllers treat the
    /// next reconcile as the resolution).
    pub fn is_already_exists(&self) -> bool {
        matches!(
            self,
            ControllerError::Storage(StorageError::AlreadyExists { .. })
        )
    }

    /// True when the target was missing (benign: the object was deleted
    /// concurrently; the informer forgets the key naturally).
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            ControllerError::Storage(StorageError::NotFound { .. })
        )
    }
}
