//! The generic [`StorageBackend`] trait (T2.2/T2.3) + [`Watch`] handle.

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::entry::{Revision, StoredEntry, WatchEvent};
use crate::error::StorageError;
use crate::key::{Key, KeyPrefix};

/// A live watch handle. Yields [`WatchEvent`]s whose key falls under the watch
/// prefix, from subscription onward.
pub struct Watch {
    rx: broadcast::Receiver<WatchEvent>,
    prefix: String,
}

impl Watch {
    /// Construct from a broadcast receiver + the prefix to filter on.
    pub(crate) fn new(rx: broadcast::Receiver<WatchEvent>, prefix: String) -> Self {
        Self { rx, prefix }
    }

    /// Receive the next matching event, or `None` when the backend is dropped.
    ///
    /// Events whose key does not start with the watch prefix are skipped; brief
    /// lag bursts (a slow consumer missing events) are tolerated by skipping
    /// rather than erroring.
    pub async fn recv(&mut self) -> Option<WatchEvent> {
        loop {
            match self.rx.recv().await {
                Ok(ev) if event_in_prefix(&ev, &self.prefix) => return Some(ev),
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    }
}

fn event_in_prefix(ev: &WatchEvent, prefix: &str) -> bool {
    let key = match ev {
        WatchEvent::Put(e) => e.key.as_str(),
        WatchEvent::Delete { key, .. } => key.as_str(),
    };
    key.starts_with(prefix)
}

/// Generic resource storage contract (etcd / SQLite-KINE / embedded).
///
/// All writes bump a single monotonic [`Revision`]; optimistic concurrency is
/// enforced via the `if_revision` CAS parameter (`Some` = check, `None` =
/// blind). The `mod_revision` of the returned [`StoredEntry`] is the
/// Kubernetes `resourceVersion`.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Create a new object at `key`. Fails with [`StorageError::AlreadyExists`]
    /// if the key is occupied.
    async fn create(
        &self,
        key: &Key,
        value: serde_json::Value,
    ) -> Result<StoredEntry, StorageError>;

    /// Fetch a single object, or `None` if absent.
    async fn get(&self, key: &Key) -> Result<Option<StoredEntry>, StorageError>;

    /// List every object under `prefix`, ordered by `mod_revision`.
    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<StoredEntry>, StorageError>;

    /// Update an existing object. `if_revision = Some(r)` enforces optimistic
    /// concurrency (the current `mod_revision` must equal `r`).
    async fn update(
        &self,
        key: &Key,
        value: serde_json::Value,
        if_revision: Option<Revision>,
    ) -> Result<StoredEntry, StorageError>;

    /// Delete an object, returning the removed entry (or `None` if absent).
    /// `if_revision` enforces optimistic concurrency.
    async fn delete(
        &self,
        key: &Key,
        if_revision: Option<Revision>,
    ) -> Result<Option<StoredEntry>, StorageError>;

    /// The highest cluster revision assigned so far.
    async fn current_revision(&self) -> Result<Revision, StorageError>;

    /// Subscribe to live events under `prefix`. `start_revision` is accepted
    /// for API parity with etcd (historical replay is a future etcd-backend
    /// capability; the embedded backend delivers events from subscription).
    async fn watch(
        &self,
        prefix: &KeyPrefix,
        start_revision: Option<Revision>,
    ) -> Result<Watch, StorageError>;
}
