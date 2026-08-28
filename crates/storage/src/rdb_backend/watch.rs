//! Watch historical replay + compaction for [`super::RdbStorage`]
//! (T2.3/Q33, semantics from T2.2/Q29).
//!
//! The `log/` prefix IS the event log, so replay is a range scan
//! (`log/<start BE>` forward, revision-ordered by key layout) and compaction
//! is a bounded delete of `log/<rev>` plus the persisted watermark -- both
//! written in ONE atomic `WriteBatch`. Both helpers run while the caller
//! holds the backend state mutex (see [`super`]), which is what makes the
//! replay -> live seam lossless: post-COMMIT broadcasts happen under that
//! same mutex, so no event can fall between the subscription and the scan.

use std::sync::Arc;

use rocksdb::WriteBatch;
use rdb::store::ops;
use rdb::store::Store;
use tokio::sync::broadcast;

use super::keys::{self, LogRecord, META_COMPACT};
use crate::backend::{event_in_prefix, Watch};
use crate::entry::{Revision, WatchEvent};
use crate::error::StorageError;

/// Build a replay-then-live watch starting at revision `n` (inclusive),
/// mirroring the shared backend semantics 1:1. The caller holds the
/// state mutex: subscribe-before-scan plus commit-then-broadcast under the
/// same mutex keep the seam duplicate-free and gap-free (ordering ==
/// revision order).
pub(super) fn replay(
    db: &Store,
    tx: &broadcast::Sender<WatchEvent>,
    path: &str,
    n: Revision,
    watermark: Revision,
) -> Result<Watch, StorageError> {
    if n <= watermark {
        // etcd ErrCompacted; upstream watch 410 Gone.
        return Err(StorageError::Compacted {
            requested: n,
            watermark,
        });
    }
    // Subscribe BEFORE scanning: events committed from here on reach this
    // receiver through the broadcast only (writers are blocked on the state
    // mutex until the scan completes), so nothing is replayed twice.
    let rx = tx.subscribe();
    let from = keys::log_key(n); // inclusive lower bound: `start_revision` inclusive
    let mut replayed: Vec<WatchEvent> = Vec::new();
    let mut bad: Option<StorageError> = None;
    let scan = ops::for_each_from(db, &from, false, &mut |k, v| {
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
    Ok(Watch::with_replay(rx, path.to_string(), replayed, n))
}

/// Advance the compaction watermark to `target`: delete every retained
/// `log/<rev>` record at or below it AND persist the new watermark in one
/// atomic `WriteBatch` (SQLite compacts watermark + rows in one `BEGIN
/// IMMEDIATE` tx; same durability semantics, different engine). Live objects
/// live under `obj/`, so reads are unaffected (trait contract).
///
/// The caller folds `target` to the current revision (monotonic, never
/// lowered) and bumps the in-memory watermark only after this commit wins.
pub(super) async fn compact(db: Arc<Store>, target: Revision) -> Result<(), StorageError> {
    let upper = keys::log_key(target); // inclusive upper bound
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
    batch.put(keys::meta_key(META_COMPACT), target.to_be_bytes());
    // Same commit discipline as the CRUD path: one synchronous batch, fsync
    // parked on the blocking pool, broadcast-free (compaction emits no event).
    ops::batch_write_async(db, batch)
        .await
        .map_err(|e| StorageError::Backend(format!("compaction commit: {e}")))
}
