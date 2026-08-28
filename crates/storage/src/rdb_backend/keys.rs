//! Single-CF RocksDB key layout + record codecs for [`super::RdbStorage`]
//! (T2.3/Q33).
//!
//! The SQLite backend (T2.3/Q29) maps the etcd semantics onto one append-only
//! `kv` table; this layout keeps the SAME record model, spread over
//! lexicographic prefixes of the default column family so prefix scans stand
//! in for SQL ranges:
//!
//! | physical key         | value                                            |
//! |----------------------|--------------------------------------------------|
//! | `meta/revision`      | cluster revision, 8-byte big-endian `u64`        |
//! | `meta/compact`       | compaction watermark (lowest retained log rev)   |
//! | `obj/<key>`          | live [`StoredEntry`] as JSON (deleted = absent)  |
//! | `log/<rev BE u64>`   | one [`LogRecord`] JSON per successful write      |
//!
//! `log/` keys are fixed-width big-endian so lexicographic order IS revision
//! order: a range scan over `log/` is a revision-ordered replay, and
//! "delete everything at or below the watermark" is a bounded prefix range.
//! `obj/` and `log/` are separate (unlike SQLite, where the row table IS the
//! log), so compaction can drop history without touching live reads.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use rdb::store::ops::get_physical;
use rdb::store::Store;

use crate::entry::{Revision, StoredEntry, WatchEvent};
use crate::error::StorageError;

pub(super) const META_PREFIX: &[u8] = b"meta/";
pub(super) const META_REVISION: &[u8] = b"revision";
pub(super) const META_COMPACT: &[u8] = b"compact";
pub(super) const OBJ_PREFIX: &[u8] = b"obj/";
pub(super) const LOG_PREFIX: &[u8] = b"log/";

/// Full physical key of a live object: `obj/` + the `/registry/...` path.
pub(super) fn obj_key(path: &str) -> Vec<u8> {
    suffixed(OBJ_PREFIX, path.as_bytes())
}

/// Lower bound for a list scan: `obj/` + the prefix path. Callers append the
/// trailing `/` to the [`crate::KeyPrefix`] path first (segment-delimiting,
/// embedded/sqlite parity: `/registry/pods` must not match `/registry/podsabc`).
pub(super) fn obj_list_prefix(prefixed_path: &str) -> Vec<u8> {
    suffixed(OBJ_PREFIX, prefixed_path.as_bytes())
}

/// Physical log key of one revision: `log/` + big-endian `u64` (fixed width,
/// so key order == revision order).
pub(super) fn log_key(rev: Revision) -> Vec<u8> {
    suffixed(LOG_PREFIX, &rev.to_be_bytes())
}

/// Physical meta key: `meta/` + `revision` | `compact`.
pub(super) fn meta_key(suffix: &[u8]) -> Vec<u8> {
    suffixed(META_PREFIX, suffix)
}

fn suffixed(prefix: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(prefix.len() + suffix.len());
    k.extend_from_slice(prefix);
    k.extend_from_slice(suffix);
    k
}

/// Read a persisted big-endian `u64` meta record; absent -> `0` (fresh
/// datastore), wrong shape -> [`StorageError::Backend`] (corrupt record).
pub(super) fn meta_u64(db: &Store, suffix: &[u8], what: &str) -> Result<u64, StorageError> {
    let physical = meta_key(suffix);
    match get_physical(db, &physical)
        .map_err(|e| StorageError::Backend(format!("meta get {what}: {e}")))?
    {
        None => Ok(0),
        Some(v) => parse_u64(&v, what),
    }
}

/// The persisted high-water cluster revision (0 on a fresh datastore).
pub(super) fn meta_revision(db: &Store) -> Result<u64, StorageError> {
    meta_u64(db, META_REVISION, "revision")
}

/// The compaction watermark (0 = nothing compacted yet).
pub(super) fn meta_compact_revision(db: &Store) -> Result<u64, StorageError> {
    meta_u64(db, META_COMPACT, "compact_revision")
}

/// Decode an 8-byte big-endian `u64` payload.
pub(super) fn parse_u64(bytes: &[u8], what: &str) -> Result<u64, StorageError> {
    <[u8; 8]>::try_from(bytes)
        .map(u64::from_be_bytes)
        .map_err(|_| StorageError::Backend(format!("corrupt meta {what}: {} bytes", bytes.len())))
}

/// Serializable watch-history record. [`WatchEvent`] itself is not `Serialize`
/// (its payloads are `Arc`-shared), so the log persists this mirror: the same
/// information a SQLite tombstone/put row carries, one record per revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum LogRecord {
    /// A key was created or updated, carrying the full stored entry.
    Put { entry: StoredEntry },
    /// A key was deleted; `prev` is the final stored object (upstream watch
    /// `DELETED` events carry the full last state) with its PRE-delete
    /// `mod_revision`, exactly like the embedded/sqlite replay paths.
    Delete {
        key: String,
        mod_revision: Revision,
        prev: Option<StoredEntry>,
    },
}

impl LogRecord {
    /// Snapshot a broadcast event into its durable form.
    pub(super) fn of(ev: &WatchEvent) -> Self {
        match ev {
            WatchEvent::Put(e) => Self::Put {
                entry: (**e).clone(),
            },
            WatchEvent::Delete {
                key,
                mod_revision,
                prev,
            } => Self::Delete {
                key: key.clone(),
                mod_revision: *mod_revision,
                prev: prev.as_deref().cloned(),
            },
        }
    }

    /// Rebuild the broadcast event for replay (embedded/sqlite parity).
    pub(super) fn into_event(self) -> WatchEvent {
        match self {
            Self::Put { entry } => WatchEvent::Put(Arc::new(entry)),
            Self::Delete {
                key,
                mod_revision,
                prev,
            } => WatchEvent::Delete {
                key,
                mod_revision,
                prev: prev.map(Arc::new),
            },
        }
    }
}

/// Decode a stored object record (`obj/<key>` value).
pub(super) fn decode_entry(bytes: &[u8]) -> Result<StoredEntry, StorageError> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Decode a stored log record (`log/<rev>` value).
pub(super) fn decode_record(bytes: &[u8]) -> Result<LogRecord, StorageError> {
    Ok(serde_json::from_slice(bytes)?)
}
