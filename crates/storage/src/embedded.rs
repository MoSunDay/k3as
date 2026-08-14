//! In-process embedded storage backend (T2.1 spike + T2.2 default impl).
//!
//! Watch semantics (T2.2): `watch(prefix, Some(n))` replays retained history
//! from revision `n` (etcd-inclusive) and then continues with live events
//! from a single lock-ordered seam; `compact(rev)` bounds the retained
//! history (see [`crate::history`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, Mutex};

use crate::backend::{event_in_prefix, StorageBackend, Watch};
use crate::entry::{Revision, StoredEntry, WatchEvent};
use crate::error::StorageError;
use crate::history::EventLog;
use crate::key::{validate, Key, KeyPrefix};

/// Broadcast channel capacity per backend. Generous to avoid `Lagged` under
/// bursty multi-watcher workloads; a lagging consumer has its stream closed
/// so it re-lists and re-watches rather than silently missing events.
const WATCH_CAP: usize = 1024;

/// Retained watch-history length (revisions). Deliberately generous: it is
/// the disconnect window an informer can bridge with a replayed watch
/// instead of a full re-list. Events are cheap (`Arc` payloads); real
/// policy-grade retention arrives with T2.3.
const HISTORY_CAPACITY: usize = 10_000;

struct Inner {
    /// Monotonic cluster-wide revision; bumped once per successful write.
    revision: Revision,
    /// Full `/registry/...` path -> entry.
    entries: BTreeMap<String, StoredEntry>,
    /// Retained event history for watch replay + compaction (T2.2).
    log: EventLog,
    /// Fan-out for watchers. A dropped backend closes all watches.
    tx: broadcast::Sender<WatchEvent>,
}

/// Zero-dependency, in-process storage backend with etcd-faithful revision and
/// optimistic-concurrency semantics. The default backend; also the test double.
pub struct EmbeddedStorage {
    inner: Mutex<Inner>,
}

impl EmbeddedStorage {
    pub fn new() -> Self {
        Self::with_history_capacity(HISTORY_CAPACITY)
    }

    /// Like [`new`] but with an explicit retained-history capacity (exposed
    /// for tests and retention tuning).
    pub fn with_history_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel::<WatchEvent>(WATCH_CAP);
        Self {
            inner: Mutex::new(Inner {
                revision: 0,
                entries: BTreeMap::new(),
                log: EventLog::new(capacity),
                tx,
            }),
        }
    }
}

impl Default for EmbeddedStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StorageBackend for EmbeddedStorage {
    async fn create(
        &self,
        key: &Key,
        value: serde_json::Value,
    ) -> Result<StoredEntry, StorageError> {
        validate(key)?;
        let path = key.as_path();
        let mut g = self.inner.lock().await;
        if g.entries.contains_key(&path) {
            return Err(StorageError::AlreadyExists { key: path });
        }
        g.revision += 1;
        let rev = g.revision;
        let entry = StoredEntry {
            key: path.clone(),
            value,
            create_revision: rev,
            mod_revision: rev,
            version: 1,
        };
        g.entries.insert(path.clone(), entry.clone());
        let ev = WatchEvent::Put(Arc::new(entry.clone()));
        g.log.push(rev, ev.clone());
        let _ = g.tx.send(ev);
        Ok(entry)
    }

    async fn get(&self, key: &Key) -> Result<Option<StoredEntry>, StorageError> {
        let g = self.inner.lock().await;
        Ok(g.entries.get(&key.as_path()).cloned())
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<StoredEntry>, StorageError> {
        // Append "/" to delimit path segments so "/registry/pods" does not
        // match "/registry/podsabc".
        let p = format!("{}/", prefix.as_path());
        let g = self.inner.lock().await;
        let mut out: Vec<StoredEntry> = g
            .entries
            .range(p.clone()..)
            .take_while(|(k, _)| k.starts_with(&p))
            .map(|(_, v)| v.clone())
            .collect();
        drop(g);
        // BTreeMap iter is key-sorted; sort by mod_revision for a stable,
        // resourceVersion-ordered list (matches k8s LIST ordering by RV).
        out.sort_by_key(|e| e.mod_revision);
        Ok(out)
    }

    async fn update(
        &self,
        key: &Key,
        value: serde_json::Value,
        if_revision: Option<Revision>,
    ) -> Result<StoredEntry, StorageError> {
        let path = key.as_path();
        let mut g = self.inner.lock().await;
        let cur = match g.entries.get(&path) {
            Some(e) => e.clone(),
            None => return Err(StorageError::NotFound { key: path }),
        };
        if let Some(want) = if_revision {
            if cur.mod_revision != want {
                return Err(StorageError::Conflict {
                    key: path,
                    expected: if_revision,
                    have: Some(cur.mod_revision),
                });
            }
        }
        g.revision += 1;
        let rev = g.revision;
        let updated = StoredEntry {
            key: path.clone(),
            value,
            create_revision: cur.create_revision,
            mod_revision: rev,
            version: cur.version + 1,
        };
        g.entries.insert(path.clone(), updated.clone());
        let ev = WatchEvent::Put(Arc::new(updated.clone()));
        g.log.push(rev, ev.clone());
        let _ = g.tx.send(ev);
        Ok(updated)
    }

    async fn delete(
        &self,
        key: &Key,
        if_revision: Option<Revision>,
    ) -> Result<Option<StoredEntry>, StorageError> {
        let path = key.as_path();
        let mut g = self.inner.lock().await;
        let cur = match g.entries.get(&path) {
            Some(e) => e.clone(),
            None => return Ok(None),
        };
        if let Some(want) = if_revision {
            if cur.mod_revision != want {
                return Err(StorageError::Conflict {
                    key: path,
                    expected: if_revision,
                    have: Some(cur.mod_revision),
                });
            }
        }
        g.revision += 1;
        let rev = g.revision;
        g.entries.remove(&path);
        let ev = WatchEvent::Delete {
            key: path,
            mod_revision: rev,
            prev: Some(Arc::new(cur.clone())),
        };
        g.log.push(rev, ev.clone());
        let _ = g.tx.send(ev);
        Ok(Some(cur))
    }

    async fn current_revision(&self) -> Result<Revision, StorageError> {
        Ok(self.inner.lock().await.revision)
    }

    async fn watch(
        &self,
        prefix: &KeyPrefix,
        start_revision: Option<Revision>,
    ) -> Result<Watch, StorageError> {
        let g = self.inner.lock().await;
        let p = format!("{}/", prefix.as_path());
        match start_revision {
            None | Some(0) => Ok(Watch::live(g.tx.subscribe(), p)),
            Some(n) => {
                // Snapshot history AND subscribe under one lock: events
                // pushed after this point flow via the broadcast only, so the
                // replay -> live seam is lossless and duplicate-free.
                let mut replay = g.log.since(n)?;
                replay.retain(|ev| event_in_prefix(ev, &p));
                Ok(Watch::with_replay(g.tx.subscribe(), p, replay, n))
            }
        }
    }

    async fn compact(&self, revision: Revision) -> Result<Revision, StorageError> {
        let mut g = self.inner.lock().await;
        // Fold future requests to the current revision (documented on the
        // trait): a periodic policy would reach this watermark anyway.
        let target = revision.min(g.revision);
        Ok(g.log.compact(target))
    }
}
