//! Blocking-pool twins of the synchronous RocksDB scans (T2.3/Q33
//! hardening): the `obj/` prefix LIST, the `log/` watch replay and the
//! compaction range scan are plain iterators, so each runs under
//! `tokio::task::spawn_blocking` -- never on the reactor thread. (The
//! durability/replay tests drive this backend on a current-thread runtime,
//! where `block_in_place` would panic, hence the explicit pool hop.)
//!
//! The pure `&Store` functions do the work; the async wrappers own an
//! `Arc<Store>` clone and fold a join failure into [`StorageError::Backend`].
//! Locking is unchanged: callers keep whatever mutex they held across the
//! await -- the replay seam (T2.2/Q29) needs exactly that.

use std::sync::Arc;

use rdb::store::{ops, Store};
use rocksdb::WriteBatch;

use super::keys::{self, LogRecord};
use crate::backend::event_in_prefix;
use crate::entry::{StoredEntry, WatchEvent};
use crate::error::StorageError;

/// A blocking-pool join failure (panic or cancel) is a backend failure.
fn join_failed(what: &str, e: tokio::task::JoinError) -> StorageError {
    StorageError::Backend(format!("{what} task: {e}"))
}

/// `obj/` prefix scan behind [`crate::StorageBackend::list`] (blocking pool).
pub(super) async fn list_entries(
    db: Arc<Store>,
    lo: Vec<u8>,
) -> Result<Vec<StoredEntry>, StorageError> {
    tokio::task::spawn_blocking(move || scan_entries(&db, &lo))
        .await
        .map_err(|e| join_failed("list scan", e))?
}

/// Pure `obj/` prefix scan: key-sorted by RocksDB, then re-sorted by
/// `mod_revision` for the stable, resourceVersion-ordered LIST (matches k8s
/// LIST ordering by RV). A record that fails to decode fails the whole LIST
/// -- never a partial answer.
pub(super) fn scan_entries(db: &Store, lo: &[u8]) -> Result<Vec<StoredEntry>, StorageError> {
    let mut out: Vec<StoredEntry> = Vec::new();
    let mut bad: Option<StorageError> = None;
    let scan = ops::for_each_from(db, lo, false, &mut |k, v| {
        if !k.starts_with(lo) {
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
    out.sort_by_key(|e| e.mod_revision);
    Ok(out)
}

/// `log/` replay scan behind watch replay (blocking pool).
pub(super) async fn replay_events(
    db: Arc<Store>,
    from: Vec<u8>,
    path: String,
) -> Result<Vec<WatchEvent>, StorageError> {
    tokio::task::spawn_blocking(move || scan_replay(&db, &from, &path))
        .await
        .map_err(|e| join_failed("watch replay", e))?
}

/// Pure `log/` range scan from `from` (inclusive; revision-ordered by key
/// layout), keeping the events under `path`. The first record past the end
/// of `log/` ends the scan; a record that fails to decode fails the replay.
pub(super) fn scan_replay(
    db: &Store,
    from: &[u8],
    path: &str,
) -> Result<Vec<WatchEvent>, StorageError> {
    let mut replayed: Vec<WatchEvent> = Vec::new();
    let mut bad: Option<StorageError> = None;
    let scan = ops::for_each_from(db, from, false, &mut |k, v| {
        if !k.starts_with(keys::LOG_PREFIX) {
            return false; // ran past the log range
        }
        match keys::decode_record(v).map(LogRecord::into_event) {
            Ok(ev) => {
                if event_in_prefix(&ev, path) {
                    replayed.push(ev);
                }
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
    scan.map_err(|e| StorageError::Backend(format!("watch replay scan: {e}")))?;
    Ok(replayed)
}

/// Compaction range scan (blocking pool): every `log/` record at or below
/// `upper` (inclusive) as delete tombstones in one `WriteBatch`.
pub(super) async fn compaction_batch(
    db: Arc<Store>,
    upper: Vec<u8>,
) -> Result<WriteBatch, StorageError> {
    tokio::task::spawn_blocking(move || {
        let mut batch = WriteBatch::default();
        let scan = ops::for_each_from(&db, keys::LOG_PREFIX, false, &mut |k, _| {
            if !k.starts_with(keys::LOG_PREFIX) {
                return false;
            }
            if *k > *upper {
                return false; // first record above the new watermark
            }
            batch.delete(k);
            true
        });
        scan.map_err(|e| StorageError::Backend(format!("compaction scan: {e}")))?;
        Ok(batch)
    })
    .await
    .map_err(|e| join_failed("compaction scan", e))?
}
