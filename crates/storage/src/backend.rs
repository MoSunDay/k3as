//! The generic [`StorageBackend`] trait (T2.2/T2.3) + [`Watch`] handle.

use std::collections::VecDeque;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::entry::{Revision, StoredEntry, WatchEvent};
use crate::error::StorageError;
use crate::key::{Key, KeyPrefix};

/// A watch handle: optional replayed history first, then the live stream.
///
/// Constructed by a backend while holding its write lock, so the replay
/// snapshot and the broadcast subscription share one seam -- every event is
/// delivered exactly once, in revision order (T2.2).
pub struct Watch {
    rx: broadcast::Receiver<WatchEvent>,
    prefix: String,
    /// Snapshotted history (prefix-filtered), drained before the live stream.
    replay: VecDeque<WatchEvent>,
    /// Lowest live revision to deliver; guards the replay/live seam and
    /// future `start_revision`s.
    min_revision: Revision,
}

impl Watch {
    /// Live-only watch: events from subscription onward.
    pub(crate) fn live(rx: broadcast::Receiver<WatchEvent>, prefix: String) -> Self {
        Self {
            rx,
            prefix,
            replay: VecDeque::new(),
            min_revision: 0,
        }
    }

    /// Replay-then-live watch. `replay` holds prefix-filtered events at
    /// revisions `>= min_revision` already snapshotted; live events below
    /// `min_revision` (and any re-delivery at the seam) are skipped.
    pub(crate) fn with_replay(
        rx: broadcast::Receiver<WatchEvent>,
        prefix: String,
        replay: Vec<WatchEvent>,
        min_revision: Revision,
    ) -> Self {
        Self {
            rx,
            prefix,
            replay: replay.into(),
            min_revision,
        }
    }

    /// Receive the next matching event, or `None` when the stream is over.
    ///
    /// The stream closes when the backend is dropped, or when this consumer
    /// lagged past the broadcast capacity -- closing (rather than silently
    /// skipping) forces clients to re-list and re-watch, mirroring upstream
    /// informer resync semantics.
    pub async fn recv(&mut self) -> Option<WatchEvent> {
        if let Some(ev) = self.replay.pop_front() {
            return Some(ev);
        }
        loop {
            match self.rx.recv().await {
                Ok(ev) => {
                    if !event_in_prefix(&ev, &self.prefix) || ev.revision() < self.min_revision {
                        continue;
                    }
                    return Some(ev);
                }
                Err(broadcast::error::RecvError::Closed) => return None,
                Err(broadcast::error::RecvError::Lagged(_)) => return None,
            }
        }
    }
}

pub(crate) fn event_in_prefix(ev: &WatchEvent, prefix: &str) -> bool {
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

    /// Subscribe to events under `prefix`.
    ///
    /// `start_revision = Some(n)` replays retained history from revision `n`
    /// **inclusive** (etcd `start_revision` semantics), then continues with
    /// the live stream from a single seam -- no gaps, no duplicates. A start
    /// at or below the compaction watermark fails with
    /// [`StorageError::Compacted`] (upstream watch `410 Gone`). `None` (or
    /// `Some(0)`) subscribes live-only.
    async fn watch(
        &self,
        prefix: &KeyPrefix,
        start_revision: Option<Revision>,
    ) -> Result<Watch, StorageError>;

    /// Advance the history-compaction watermark to `revision`, dropping
    /// retained watch history at or below it, and return the effective
    /// watermark. Reads (`get`/`list`) are unaffected. The embedded impl
    /// clamps `revision` down to the current cluster revision (etcd rejects
    /// future compaction requests; folding to "now" reaches the same
    /// watermark a periodic policy would).
    async fn compact(&self, revision: Revision) -> Result<Revision, StorageError>;
}
