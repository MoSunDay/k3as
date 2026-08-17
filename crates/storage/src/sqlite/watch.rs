//! Watch historical replay + compaction for [`super::SqliteStorage`]
//! (T2.3, Q29).
//!
//! The `kv` table IS the event log, so replay is a range scan
//! (`id >= start_revision ORDER BY id ASC`) and compaction is row deletion
//! bounded by the persisted watermark. Both run under the connection mutex
//! (see [`super`]), which is what makes the replay -> live seam lossless.

use std::sync::Arc;

use libsql::params;
use tokio::sync::broadcast;

use super::be;
use super::schema::{self, KvRow};
use crate::backend::{event_in_prefix, Watch};
use crate::entry::{Revision, WatchEvent};
use crate::error::StorageError;

/// Every retained event row from `start_revision` in revision (= id) order.
/// Each row stores the `version` **as of that write**, so replayed `Put`
/// events carry the same version their live broadcast carried (embedded-store
/// parity).
const REPLAY_SQL: &str = "SELECT id, key, value, create_revision, prev_revision, deleted, \
     version FROM kv WHERE id >= ?1 ORDER BY id ASC";

/// Build a replay-then-live watch starting at revision `n` (inclusive),
/// mirroring `EmbeddedStorage::watch` 1:1. The caller holds the connection
/// mutex: subscribe-before-scan plus post-COMMIT broadcasts under the same
/// mutex make the seam duplicate-free (ordering == revision order).
pub(super) async fn replay(
    conn: &libsql::Connection,
    tx: &broadcast::Sender<WatchEvent>,
    path: &str,
    n: Revision,
) -> Result<Watch, StorageError> {
    let watermark = schema::meta_compact_revision(conn).await?;
    if n <= watermark {
        // etcd ErrCompacted; upstream watch 410 Gone.
        return Err(StorageError::Compacted {
            requested: n,
            watermark,
        });
    }
    // Clamp instead of wrapping: `Revision` is u64 while ids are i64; an
    // unchecked `n as i64` with n > i64::MAX wraps negative and would
    // replay the entire log, where embedded replays nothing (live-only).
    // i64::MAX matches no row id, reproducing that seam (deep-review #4).
    let start = i64::try_from(n).unwrap_or(i64::MAX);
    let rx = tx.subscribe();
    let mut rows = conn.query(REPLAY_SQL, params![start]).await.map_err(be)?;
    let mut replayed = Vec::new();
    while let Some(row) = rows.next().await.map_err(be)? {
        let kv = KvRow::from_row(&row)?;
        replayed.push(row_event(&kv)?);
    }
    replayed.retain(|ev| event_in_prefix(ev, path));
    Ok(Watch::with_replay(rx, path.to_string(), replayed, n))
}

/// Map one event row to its [`WatchEvent`]. Tombstone rows replay as
/// `Delete` events whose `prev` is the pre-delete entry: the tombstone
/// carries the final object, and its `prev_revision` is the deleted
/// entry's `mod_revision` (embedded-store parity -- upstream watch
/// `DELETED` events carry the full last state).
fn row_event(kv: &KvRow) -> Result<WatchEvent, StorageError> {
    if kv.deleted {
        let mut prev = schema::row_to_entry(kv)?;
        prev.mod_revision = kv.prev_revision.max(0) as u64;
        Ok(WatchEvent::Delete {
            key: kv.key.clone(),
            mod_revision: kv.id as u64,
            prev: Some(Arc::new(prev)),
        })
    } else {
        Ok(WatchEvent::Put(Arc::new(schema::row_to_entry(kv)?)))
    }
}

/// Advance the compaction watermark to `revision` (folded down to the
/// current cluster revision; monotonic, never lowered) and return the
/// effective watermark. The latest row per key is always retained, so
/// `get`/`list` results are unaffected (trait contract); `version` is
/// stored per row, so it too is unaffected by dropped history.
pub(super) async fn compact(
    conn: &libsql::Connection,
    revision: Revision,
) -> Result<Revision, StorageError> {
    super::begin_write_tx(conn).await?;
    super::finish_tx(conn, compact_tx(conn, revision).await).await
}

async fn compact_tx(
    conn: &libsql::Connection,
    revision: Revision,
) -> Result<Revision, StorageError> {
    // Fold future requests to the current revision (documented on the
    // trait): a periodic policy would reach this watermark anyway.
    let current = schema::meta_revision(conn).await?;
    let target = revision.min(current);
    let watermark = schema::meta_compact_revision(conn).await?;
    if target > watermark {
        conn.execute(
            "DELETE FROM kv WHERE id <= ?1 \
             AND id NOT IN (SELECT MAX(id) FROM kv GROUP BY key)",
            params![target as i64],
        )
        .await
        .map_err(be)?;
        schema::meta_set(conn, schema::META_COMPACT_REVISION, &target.to_string()).await?;
        return Ok(target);
    }
    Ok(watermark)
}
