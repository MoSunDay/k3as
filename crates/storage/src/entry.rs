//! Stored value + revision bookkeeping (etcd `KeyValue` parity, T2.2).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Monotonic cluster-wide revision number (etcd `revision`).
pub type Revision = u64;

/// One stored resource, mirroring etcd's `KeyValue`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredEntry {
    /// Full `/registry/...` path.
    pub key: String,
    /// Raw object payload (canonical JSON).
    pub value: serde_json::Value,
    /// Revision at which the key was first created.
    pub create_revision: Revision,
    /// Revision of the last modification -- the Kubernetes `resourceVersion`.
    pub mod_revision: Revision,
    /// Count of writes to this key since creation (etcd `version`).
    pub version: u64,
}

/// An event delivered to a [`crate::Watch`].
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// A key was created or updated.
    Put(Arc<StoredEntry>),
    /// A key was deleted. `prev` is the final stored object (upstream watch
    /// `DELETED` events carry the full last state); `mod_revision` is the
    /// deletion revision.
    Delete {
        key: String,
        mod_revision: Revision,
        prev: Option<Arc<StoredEntry>>,
    },
}

impl WatchEvent {
    /// The cluster revision this event occurred at.
    pub fn revision(&self) -> Revision {
        match self {
            WatchEvent::Put(e) => e.mod_revision,
            WatchEvent::Delete { mod_revision, .. } => *mod_revision,
        }
    }
}
