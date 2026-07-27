//! In-process embedded storage backend (T2.1 spike + T2.2 default impl).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, Mutex};

use crate::backend::{StorageBackend, Watch};
use crate::entry::{Revision, StoredEntry, WatchEvent};
use crate::error::StorageError;
use crate::key::{validate, Key, KeyPrefix};

/// Broadcast channel capacity per backend. Generous to avoid `Lagged` under
/// bursty multi-watcher workloads; the watch handle tolerates lag by skipping.
const WATCH_CAP: usize = 1024;

struct Inner {
    /// Monotonic cluster-wide revision; bumped once per successful write.
    revision: Revision,
    /// Full `/registry/...` path -> entry.
    entries: BTreeMap<String, StoredEntry>,
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
        let (tx, _rx) = broadcast::channel::<WatchEvent>(WATCH_CAP);
        Self {
            inner: Mutex::new(Inner {
                revision: 0,
                entries: BTreeMap::new(),
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
        let _ = g.tx.send(WatchEvent::Put(Arc::new(entry.clone())));
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
        let _ = g.tx.send(WatchEvent::Put(Arc::new(updated.clone())));
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
        let _ = g.tx.send(WatchEvent::Delete {
            key: path,
            mod_revision: rev,
        });
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
        // start_revision is accepted for API parity; the embedded backend
        // delivers live events from subscription (historical replay is an
        // etcd-backend capability).
        let _ = start_revision;
        let g = self.inner.lock().await;
        let rx = g.tx.subscribe();
        let p = format!("{}/", prefix.as_path());
        Ok(Watch::new(rx, p))
    }
}
