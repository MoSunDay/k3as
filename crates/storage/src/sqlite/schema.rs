//! SQLite schema + row plumbing for [`super::SqliteStorage`] (T2.3, Q29).
//!
//! kine-style append-only event table: every write (create / update /
//! delete) appends exactly one `kv` row whose `id` **is** the cluster
//! revision. Latest state per key = the `MAX(id)` row for that key;
//! tombstones are `deleted = 1` rows. The `meta` table persists the
//! high-water revision counter (restored on open) and the compaction
//! watermark. Each row also stores its etcd `version` explicitly (1 at
//! create, +1 per update), so reads and replays never re-derive it and
//! compaction cannot alter it (embedded-store parity, S3 fix).

use libsql::params;

use crate::entry::StoredEntry;
use crate::error::StorageError;

/// Idempotent DDL, applied on every open (T2.3). `version` is stored
/// explicitly per event row (S3 fix): deriving it from COUNT(*) over the
/// retained generation rows made it shrink after `compact()` dropped
/// superseded rows, diverging from `EmbeddedStorage`. The DDL stays at
/// schema_version "1" (same unreleased sprint; no migration needed).
pub(super) const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS kv (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        key TEXT NOT NULL,
        value TEXT NOT NULL,
        create_revision INTEGER NOT NULL DEFAULT 0,
        prev_revision INTEGER NOT NULL DEFAULT 0,
        deleted INTEGER NOT NULL DEFAULT 0,
        version INTEGER NOT NULL DEFAULT 1
    )",
    "CREATE INDEX IF NOT EXISTS kv_key_id_idx ON kv (key, id)",
    "CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
];

/// Meta keys (single-row config table).
pub(super) const META_SCHEMA_VERSION: &str = "schema_version";
pub(super) const META_REVISION: &str = "revision";
pub(super) const META_COMPACT_REVISION: &str = "compact_revision";

/// Latest row for one key, any generation (including tombstones). The
/// column order here (and in the list/replay queries, qualified) is what
/// [`KvRow::from_row`] decodes by index.
pub(super) const LATEST_ROW_SQL: &str =
    "SELECT id, key, value, create_revision, prev_revision, deleted, version FROM kv \
     WHERE key = ?1 ORDER BY id DESC LIMIT 1";

/// Next cluster revision, allocated inside an open write transaction.
pub(super) const NEXT_REV_SQL: &str = "SELECT COALESCE(MAX(id), 0) + 1 FROM kv";

/// Append one event row (the only write statement in the backend). The
/// caller supplies the etcd `version` of the generation: 1 at create,
/// `cur + 1` on update, carried unchanged by the delete tombstone.
pub(super) const INSERT_ROW_SQL: &str =
    "INSERT INTO kv (id, key, value, create_revision, prev_revision, deleted, version) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";

/// List every live object under a prefix, latest row per key, in
/// mod_revision (= id) order; `version` is read straight off the row.
pub(super) const LIST_SQL: &str = "SELECT kv.id, kv.key, kv.value, kv.create_revision, \
     kv.prev_revision, kv.deleted, kv.version \
     FROM kv \
     WHERE kv.key >= ?1 AND (?2 IS NULL OR kv.key < ?2) AND kv.deleted = 0 \
       AND kv.id = (SELECT MAX(id) FROM kv k2 WHERE k2.key = kv.key) \
     ORDER BY kv.id ASC";

/// Persisted high-water `MAX(id)` (used to restore the revision counter).
pub(super) const MAX_ID_SQL: &str = "SELECT COALESCE(MAX(id), 0) FROM kv";

/// One decoded `kv` event row (private to the backend).
pub(super) struct KvRow {
    pub(super) id: i64,
    pub(super) key: String,
    pub(super) value: String,
    pub(super) create_revision: i64,
    pub(super) prev_revision: i64,
    pub(super) deleted: bool,
    /// Stored etcd `version` of this write: 1 at create, +1 per update,
    /// carried unchanged by the delete tombstone (S3 fix: previously
    /// derived via COUNT subqueries, which diverged after compaction).
    pub(super) version: i64,
}

impl KvRow {
    /// Decode the seven selected columns by index.
    pub(super) fn from_row(row: &libsql::Row) -> Result<Self, StorageError> {
        Ok(Self {
            id: row.get::<i64>(0).map_err(super::be)?,
            key: row.get::<String>(1).map_err(super::be)?,
            value: row.get::<String>(2).map_err(super::be)?,
            create_revision: row.get::<i64>(3).map_err(super::be)?,
            prev_revision: row.get::<i64>(4).map_err(super::be)?,
            deleted: row.get::<i64>(5).map_err(super::be)? != 0,
            version: row.get::<i64>(6).map_err(super::be)?,
        })
    }
}

/// Byte-range upper bound for a prefix: increment the last byte. Prefixes
/// here always end with `/`, so no carry/overflow occurs in practice; an
/// all-`0xFF` tail yields `None` (unbounded). A non-UTF-8 increment (only
/// possible for non-ASCII tails we never produce) also degrades to `None`.
pub(super) fn prefix_range(prefix: &str) -> (String, Option<String>) {
    let mut bytes = prefix.as_bytes().to_vec();
    match bytes.last_mut() {
        None => (prefix.to_string(), None),
        Some(last) if *last == 0xFF => (prefix.to_string(), None),
        Some(last) => {
            *last += 1;
            (prefix.to_string(), String::from_utf8(bytes).ok())
        }
    }
}

/// Decode a live/tombstone row into a [`StoredEntry`]; `version` is the
/// stored column (current for get/list rows, as-of-the-write for replay).
pub(super) fn row_to_entry(row: &KvRow) -> Result<StoredEntry, StorageError> {
    Ok(StoredEntry {
        key: row.key.clone(),
        value: serde_json::from_str(&row.value)?,
        create_revision: row.create_revision.max(0) as u64,
        mod_revision: row.id.max(0) as u64,
        version: row.version.max(0) as u64,
    })
}

/// Read one `meta` value.
pub(super) async fn meta_get(
    conn: &libsql::Connection,
    k: &str,
) -> Result<Option<String>, StorageError> {
    let mut rows = conn
        .query("SELECT v FROM meta WHERE k = ?1", params![k])
        .await
        .map_err(super::be)?;
    match rows.next().await.map_err(super::be)? {
        Some(row) => Ok(Some(row.get::<String>(0).map_err(super::be)?)),
        None => Ok(None),
    }
}

/// Upsert one `meta` value.
pub(super) async fn meta_set(
    conn: &libsql::Connection,
    k: &str,
    v: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO meta (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![k, v],
    )
    .await
    .map_err(super::be)?;
    Ok(())
}

/// Parse a numeric meta value; `what` names it in errors. Absent -> default.
async fn meta_u64(conn: &libsql::Connection, k: &str, what: &str) -> Result<u64, StorageError> {
    match meta_get(conn, k).await? {
        None => Ok(0),
        Some(v) => v
            .parse::<u64>()
            .map_err(|_| StorageError::Backend(format!("corrupt meta {what} value: {v:?}"))),
    }
}

/// The persisted high-water cluster revision (0 on a fresh database).
pub(super) async fn meta_revision(conn: &libsql::Connection) -> Result<u64, StorageError> {
    meta_u64(conn, META_REVISION, "revision").await
}

/// The compaction watermark (0 = nothing compacted yet).
pub(super) async fn meta_compact_revision(conn: &libsql::Connection) -> Result<u64, StorageError> {
    meta_u64(conn, META_COMPACT_REVISION, "compact_revision").await
}
