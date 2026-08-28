//! RocksDB-backed [`StorageBackend`] over the rdb `store` engine
//! (T2.3/Q33).
//!
//! The third interchangeable implementation of the etcd-faithful contract
//! (monotonic cluster revision, create/mod revision per key, CAS, watch
//! replay + compaction), selected by `--datastore-endpoint rdb://<path>`
//! (Q33). It replicates the kine-style write path 1:1 -- only
//! the durability substrate differs:
//!
//! 1. writes are serialized by one state mutex (SQLite: the connection
//!    mutex + `BEGIN IMMEDIATE`);
//! 2. the CAS check runs before the revision is bumped (embedded ordering);
//! 3. the new revision, the `obj/` record and the `log/` history record
//!    commit as ONE atomic RocksDB `WriteBatch` with `sync = true`
//!    (SQLite: one `BEGIN IMMEDIATE` .. `COMMIT` tx, `synchronous = FULL`);
//! 4. the watch event is broadcast only AFTER that commit, while the write
//!    lock is still held -- broadcast order == commit order == revision
//!    order (the seam [`crate::Watch`] replays against).
//!
//! Durability is per-write fsync (rdb `store` writes every batch with
//! `sync = true`), and the revision + compaction watermark are restored from
//! `meta/` on reopen. Single-node, local-directory mode only: the rdb raft /
//! SQL layers are compiled out behind its `store` feature (Q33 mirrors the
//! Q29 stance for SQLite -- the engine is reused, not the server).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rocksdb::WriteBatch;
use rdb::store::{open as engine_open, ops, Store};
use tokio::sync::{broadcast, Mutex};

use crate::backend::{StorageBackend, Watch};
use crate::entry::{Revision, StoredEntry, WatchEvent};
use crate::error::StorageError;
use crate::key::{validate, Key, KeyPrefix};

mod keys;
mod watch;

use keys::LogRecord;

/// Broadcast channel capacity per backend -- parity with the embedded and
/// SQLite stores' `WATCH_CAP` (same Lagged -> close semantics).
const WATCH_CAP: usize = 1024;

/// In-memory mirror of the persisted `meta/` records. Restored on [`open`]
/// and kept in lock-step with the engine: bumped only AFTER a batch commits,
/// so a failed write leaves it untouched.
struct State {
    /// Monotonic cluster-wide revision; bumped once per successful write.
    revision: Revision,
    /// Compaction watermark: the highest revision no longer retained.
    watermark: Revision,
}

/// File-backed storage on a RocksDB directory (rdb `store` engine).
/// Local-directory mode only (Q33): no replication/sync/remote features are
/// compiled in.
pub struct RdbStorage {
    /// Shared with `ops::batch_write_async`, which parks the fsync on the
    /// blocking pool. Dropping the handle releases the RocksDB directory
    /// lock (a second `open` on the same path fails cleanly).
    db: Arc<Store>,
    /// Write serialization + the replay/live seam. Held across the batch
    /// commit AND the post-commit broadcast (SQLite: the connection mutex).
    state: Mutex<State>,
    /// Fan-out for watchers. A dropped backend closes all watches.
    tx: broadcast::Sender<WatchEvent>,
}

impl RdbStorage {
    /// Open (or create) the datastore directory at `path` and restore the
    /// persisted revision + compaction watermark.
    ///
    /// Async to keep the backend-agnostic open surface (the
    /// DSN dispatch is substrate-agnostic); the engine call itself is
    /// synchronous and cheap. Opening a directory another handle still locks
    /// fails with [`StorageError::Backend`] -- never a panic.
    pub async fn open(path: &Path) -> Result<Self, StorageError> {
        // The DSN path IS the engine directory (no leaf component to spare),
        // so create the whole chain (sqlite creates the parent likewise).
        std::fs::create_dir_all(path).map_err(|e| {
            StorageError::Backend(format!("create datastore dir {}: {e}", path.display()))
        })?;
        let dir = path.to_str().ok_or_else(|| {
            StorageError::Backend(format!(
                "datastore path is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        let db = Arc::new(engine_open(dir).map_err(|e| {
            StorageError::Backend(format!("open rdb datastore at {}: {e}", path.display()))
        })?);
        // Restore the meta mirror. Unlike SQLite there is no MAX(id) fold:
        // revision and log commit in ONE atomic batch, so `meta/revision` can
        // never lag the log (no tear to defend against).
        let state = State {
            revision: keys::meta_revision(&db)?,
            watermark: keys::meta_compact_revision(&db)?,
        };
        let (tx, _rx) = broadcast::channel(WATCH_CAP);
        Ok(Self {
            db,
            state: Mutex::new(state),
            tx,
        })
    }

    /// Commit one atomic record group, fsync parked off the reactor.
    async fn commit(&self, batch: WriteBatch) -> Result<(), StorageError> {
        ops::batch_write_async(Arc::clone(&self.db), batch)
            .await
            .map_err(StorageError::Backend)
    }
}

/// One write's durable payload: the live object record, its history record
/// and the bumped revision meta, committed as a single batch.
fn write_batch(
    entry: Option<(&str, &StoredEntry)>,
    rev: Revision,
    ev: &WatchEvent,
) -> Result<WriteBatch, StorageError> {
    let mut batch = WriteBatch::default();
    if let Some((path, e)) = entry {
        batch.put(keys::obj_key(path), serde_json::to_vec(e)?);
    } else if let WatchEvent::Delete { key, .. } = ev {
        batch.delete(keys::obj_key(key));
    } else {
        return Err(StorageError::Backend(
            "internal: delete batch without a Delete event".to_string(),
        ));
    }
    batch.put(keys::log_key(rev), serde_json::to_vec(&LogRecord::of(ev))?);
    batch.put(keys::meta_key(keys::META_REVISION), rev.to_be_bytes());
    Ok(batch)
}

#[async_trait]
impl StorageBackend for RdbStorage {
    async fn create(
        &self,
        key: &Key,
        value: serde_json::Value,
    ) -> Result<StoredEntry, StorageError> {
        validate(key)?;
        let path = key.as_path();
        let mut st = self.state.lock().await;
        if ops::get_physical(&self.db, &keys::obj_key(&path))
            .map_err(StorageError::Backend)?
            .is_some()
        {
            return Err(StorageError::AlreadyExists { key: path });
        }
        let rev = st.revision + 1;
        let entry = StoredEntry {
            key: path.clone(),
            value,
            create_revision: rev,
            mod_revision: rev,
            version: 1,
        };
        let ev = WatchEvent::Put(Arc::new(entry.clone()));
        let batch = write_batch(Some((&path, &entry)), rev, &ev)?;
        self.commit(batch).await?;
        // Post-COMMIT, still under the mutex: order == revision order.
        st.revision = rev;
        let _ = self.tx.send(ev);
        Ok(entry)
    }

    async fn get(&self, key: &Key) -> Result<Option<StoredEntry>, StorageError> {
        // No lock needed: `obj/` records are whole JSON documents committed
        // atomically, so a point read never observes a torn write (SQLite
        // locks because its single connection is not thread-safe).
        let physical = keys::obj_key(&key.as_path());
        match ops::get_physical(&self.db, &physical).map_err(StorageError::Backend)? {
            None => Ok(None),
            Some(v) => Ok(Some(keys::decode_entry(&v)?)),
        }
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<StoredEntry>, StorageError> {
        // Append "/" to delimit path segments so "/registry/pods" does not
        // match "/registry/podsabc" (embedded parity).
        let lo = keys::obj_list_prefix(&format!("{}/", prefix.as_path()));
        let mut out: Vec<StoredEntry> = Vec::new();
        let mut bad: Option<StorageError> = None;
        let scan = ops::for_each_from(&self.db, &lo, false, &mut |k, v| {
            if !k.starts_with(&lo) {
                return false;
            }
            match keys::decode_entry(v) {
                Ok(e) => {
                    out.push(e);
                    true
                }
                Err(e) => {
                    bad = Some(e);
                    false
                }
            }
        });
        if let Some(e) = bad {
            return Err(e);
        }
        scan.map_err(StorageError::Backend)?;
        // Key-sorted scan; sort by mod_revision for the stable,
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
        validate(key)?;
        let path = key.as_path();
        let mut st = self.state.lock().await;
        let cur = match ops::get_physical(&self.db, &keys::obj_key(&path))
            .map_err(StorageError::Backend)?
        {
            Some(v) => keys::decode_entry(&v)?,
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
        let rev = st.revision + 1;
        let entry = StoredEntry {
            key: path.clone(),
            value,
            create_revision: cur.create_revision,
            mod_revision: rev,
            version: cur.version + 1,
        };
        let ev = WatchEvent::Put(Arc::new(entry.clone()));
        let batch = write_batch(Some((&path, &entry)), rev, &ev)?;
        self.commit(batch).await?;
        st.revision = rev;
        let _ = self.tx.send(ev);
        Ok(entry)
    }

    async fn delete(
        &self,
        key: &Key,
        if_revision: Option<Revision>,
    ) -> Result<Option<StoredEntry>, StorageError> {
        let path = key.as_path();
        let mut st = self.state.lock().await;
        let cur = match ops::get_physical(&self.db, &keys::obj_key(&path))
            .map_err(StorageError::Backend)?
        {
            Some(v) => keys::decode_entry(&v)?,
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
        let rev = st.revision + 1;
        // Tombstone carrying the final object, so replayed Delete events keep
        // the last state with its PRE-delete mod_revision (upstream parity).
        let ev = WatchEvent::Delete {
            key: path.clone(),
            mod_revision: rev,
            prev: Some(Arc::new(cur.clone())),
        };
        let batch = write_batch(None, rev, &ev)?;
        self.commit(batch).await?;
        st.revision = rev;
        let _ = self.tx.send(ev);
        Ok(Some(cur))
    }

    async fn current_revision(&self) -> Result<Revision, StorageError> {
        Ok(self.state.lock().await.revision)
    }

    async fn watch(
        &self,
        prefix: &KeyPrefix,
        start_revision: Option<Revision>,
    ) -> Result<Watch, StorageError> {
        let path = format!("{}/", prefix.as_path());
        match start_revision {
            None | Some(0) => Ok(Watch::live(self.tx.subscribe(), path)),
            Some(n) => {
                // Snapshot history AND subscribe under one lock: events
                // committed after this point flow via the broadcast only, so
                // the replay -> live seam is lossless and duplicate-free.
                let st = self.state.lock().await;
                watch::replay(&self.db, &self.tx, &path, n, st.watermark)
            }
        }
    }

    async fn compact(&self, revision: Revision) -> Result<Revision, StorageError> {
        let mut st = self.state.lock().await;
        // Fold future requests to the current revision (documented on the
        // trait): a periodic policy would reach this watermark anyway.
        let target = revision.min(st.revision);
        if target > st.watermark {
            watch::compact(Arc::clone(&self.db), target).await?;
            st.watermark = target;
            return Ok(target);
        }
        Ok(st.watermark)
    }
}
