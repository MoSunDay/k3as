//! libsql/SQLite-backed [`StorageBackend`] (T2.3, Q29).
//!
//! kine-style mapping of the etcd semantics onto one append-only `kv` table
//! whose row `id` **is** the cluster revision: create/update/delete append a
//! row inside a `BEGIN IMMEDIATE` transaction, bump the persisted
//! `meta.revision` counter in the same tx, and broadcast the watch event
//! **after COMMIT** while still holding the connection mutex -- so broadcast
//! order == commit order == revision order. Every CRUD/read/watch path
//! mirrors [`crate::embedded::EmbeddedStorage`] 1:1 (revision + version +
//! CAS + watch replay + compaction; `version` is stored explicitly per row
//! so compaction cannot alter it); only the durability substrate differs
//! (a SQLite file behind `--datastore-endpoint sqlite://...`, Q29).

use std::sync::Arc;

use async_trait::async_trait;
use libsql::params;
use tokio::sync::{broadcast, Mutex};

use crate::backend::{StorageBackend, Watch};
use crate::entry::{Revision, StoredEntry, WatchEvent};
use crate::error::StorageError;
use crate::key::{validate, Key, KeyPrefix};

mod schema;
#[cfg(test)]
mod tests;
mod watch;

/// Broadcast channel capacity per backend -- parity with the embedded
/// store's `WATCH_CAP` (same Lagged -> close semantics for slow watchers).
const WATCH_CAP: usize = 1024;

/// File-backed (or `:memory:`) storage using libsql/SQLite. Local-file mode
/// only (Q29): no remote, replication, sync, or TLS features are compiled in.
pub struct SqliteStorage {
    conn: Arc<Mutex<libsql::Connection>>,
    tx: broadcast::Sender<WatchEvent>,
}

/// Map a libsql error onto the backend-agnostic failure variant (T2.3).
fn be(e: libsql::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}

impl SqliteStorage {
    /// Open (or create) the database at `path` and restore the persisted
    /// revision counter. `:memory:` opens a private in-process database.
    pub async fn open(path: &std::path::Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| StorageError::Backend(format!("create db dir: {e}")))?;
        }
        // Local-file mode only (Q29): Builder::new_local never touches the
        // network regardless of the path string's shape.
        let db = libsql::Builder::new_local(path).build().await.map_err(be)?;
        let conn = db.connect().map_err(be)?;
        // journal_mode/busy_timeout return a result row when set, so run
        // them through query + drain; synchronous returns no rows. WAL is
        // not applicable to `:memory:` databases (the returned mode string
        // is "memory") -- the value is ignored either way.
        drain(conn.query("PRAGMA journal_mode = WAL", ()).await).await?;
        drain(conn.query("PRAGMA busy_timeout = 5000", ()).await).await?;
        conn.execute("PRAGMA synchronous = FULL", ())
            .await
            .map_err(be)?;
        for stmt in schema::DDL {
            conn.execute(stmt, ()).await.map_err(be)?;
        }
        if schema::meta_get(&conn, schema::META_SCHEMA_VERSION)
            .await?
            .is_none()
        {
            schema::meta_set(&conn, schema::META_SCHEMA_VERSION, "1").await?;
        }
        // Restore the revision counter: the persisted meta value folded
        // with the table high-water (both are written in one tx, so this
        // only guards against hand-edited databases).
        let max_id = read_i64(&conn, schema::MAX_ID_SQL).await?.max(0) as u64;
        let revision = schema::meta_revision(&conn).await?.max(max_id);
        schema::meta_set(&conn, schema::META_REVISION, &revision.to_string()).await?;
        let (tx, _rx) = broadcast::channel(WATCH_CAP);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            tx,
        })
    }
}

/// Query + consume a rows stream to completion (PRAGMAs that return
/// values when set).
async fn drain(rows: Result<libsql::Rows, libsql::Error>) -> Result<(), StorageError> {
    let mut rows = rows.map_err(be)?;
    while rows.next().await.map_err(be)?.is_some() {}
    Ok(())
}

/// Run a single-column, single-row integer SELECT.
async fn read_i64(conn: &libsql::Connection, sql: &str) -> Result<i64, StorageError> {
    let mut rows = conn.query(sql, ()).await.map_err(be)?;
    match rows.next().await.map_err(be)? {
        Some(row) => Ok(row.get::<i64>(0).map_err(be)?),
        None => Err(StorageError::Backend(format!("no row for: {sql}"))),
    }
}

/// Latest row for `key`, any generation (including tombstones).
async fn latest_row(
    conn: &libsql::Connection,
    key: &str,
) -> Result<Option<schema::KvRow>, StorageError> {
    let mut rows = conn
        .query(schema::LATEST_ROW_SQL, params![key])
        .await
        .map_err(be)?;
    match rows.next().await.map_err(be)? {
        Some(row) => Ok(Some(schema::KvRow::from_row(&row)?)),
        None => Ok(None),
    }
}

/// Begin a write transaction, self-healing first: a write future dropped
/// (aborted) between BEGIN and COMMIT -- or a COMMIT that itself failed --
/// leaves the transaction open on the single shared connection, so every
/// later write would fail until process restart (deep-review #2/#3). A
/// best-effort ROLLBACK discards only uncommitted work; commit order ==
/// revision order is still guaranteed by the connection mutex.
async fn begin_write_tx(conn: &libsql::Connection) -> Result<(), StorageError> {
    if !conn.is_autocommit() {
        let _ = conn.execute("ROLLBACK", ()).await;
    }
    conn.execute("BEGIN IMMEDIATE", ()).await.map_err(be)?;
    Ok(())
}

/// Finish an open write transaction: COMMIT on success, best-effort
/// ROLLBACK on error -- including a failed COMMIT, which would otherwise
/// leave the transaction open on the connection (deep-review #2). Callers
/// broadcast only after a successful COMMIT, while still holding the
/// connection mutex.
async fn finish_tx<T>(
    conn: &libsql::Connection,
    r: Result<T, StorageError>,
) -> Result<T, StorageError> {
    match r {
        Ok(v) => match conn.execute("COMMIT", ()).await {
            Ok(_) => Ok(v),
            Err(e) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                Err(be(e))
            }
        },
        Err(e) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(e)
        }
    }
}

#[async_trait]
impl StorageBackend for SqliteStorage {
    async fn create(
        &self,
        key: &Key,
        value: serde_json::Value,
    ) -> Result<StoredEntry, StorageError> {
        validate(key)?;
        let path = key.as_path();
        let conn = self.conn.lock().await;
        begin_write_tx(&conn).await?;
        let (entry, ev) = finish_tx(&conn, create_tx(&conn, &path, &value).await).await?;
        // Broadcast post-COMMIT under the mutex: order == revision order.
        let _ = self.tx.send(ev);
        Ok(entry)
    }

    async fn get(&self, key: &Key) -> Result<Option<StoredEntry>, StorageError> {
        let conn = self.conn.lock().await;
        let path = key.as_path();
        let Some(cur) = latest_row(&conn, &path).await?.filter(|r| !r.deleted) else {
            return Ok(None);
        };
        Ok(Some(schema::row_to_entry(&cur)?))
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<StoredEntry>, StorageError> {
        // Append "/" to delimit path segments so "/registry/pods" does not
        // match "/registry/podsabc" (embedded parity).
        let p = format!("{}/", prefix.as_path());
        let (lo, hi) = schema::prefix_range(&p);
        let hi = hi.map(libsql::Value::from).unwrap_or(libsql::Value::Null);
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(schema::LIST_SQL, params![lo, hi])
            .await
            .map_err(be)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(be)? {
            let kv = schema::KvRow::from_row(&row)?;
            out.push(schema::row_to_entry(&kv)?);
        }
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
        let conn = self.conn.lock().await;
        begin_write_tx(&conn).await?;
        let (entry, ev) =
            finish_tx(&conn, update_tx(&conn, &path, &value, if_revision).await).await?;
        let _ = self.tx.send(ev);
        Ok(entry)
    }

    async fn delete(
        &self,
        key: &Key,
        if_revision: Option<Revision>,
    ) -> Result<Option<StoredEntry>, StorageError> {
        let path = key.as_path();
        let conn = self.conn.lock().await;
        begin_write_tx(&conn).await?;
        let removed = finish_tx(&conn, delete_tx(&conn, &path, if_revision).await).await?;
        if let Some((entry, ev)) = removed {
            let _ = self.tx.send(ev);
            return Ok(Some(entry));
        }
        Ok(None)
    }

    async fn current_revision(&self) -> Result<Revision, StorageError> {
        let conn = self.conn.lock().await;
        schema::meta_revision(&conn).await
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
                let conn = self.conn.lock().await;
                watch::replay(&conn, &self.tx, &path, n).await
            }
        }
    }

    async fn compact(&self, revision: Revision) -> Result<Revision, StorageError> {
        let conn = self.conn.lock().await;
        watch::compact(&conn, revision).await
    }
}

/// Create body (inside an open tx): appends the generation's first row.
async fn create_tx(
    conn: &libsql::Connection,
    path: &str,
    value: &serde_json::Value,
) -> Result<(StoredEntry, WatchEvent), StorageError> {
    if let Some(cur) = latest_row(conn, path).await? {
        if !cur.deleted {
            return Err(StorageError::AlreadyExists {
                key: path.to_string(),
            });
        }
    }
    let next = read_i64(conn, schema::NEXT_REV_SQL).await?;
    conn.execute(
        schema::INSERT_ROW_SQL,
        params![next, path, value.to_string(), next, 0i64, 0i64, 1i64],
    )
    .await
    .map_err(be)?;
    schema::meta_set(conn, schema::META_REVISION, &next.to_string()).await?;
    let entry = StoredEntry {
        key: path.to_string(),
        value: value.clone(),
        create_revision: next as u64,
        mod_revision: next as u64,
        version: 1,
    };
    Ok((entry.clone(), WatchEvent::Put(Arc::new(entry))))
}

/// Update body (inside an open tx): CAS on the latest row's id.
async fn update_tx(
    conn: &libsql::Connection,
    path: &str,
    value: &serde_json::Value,
    if_revision: Option<Revision>,
) -> Result<(StoredEntry, WatchEvent), StorageError> {
    let cur = match latest_row(conn, path).await? {
        Some(row) if !row.deleted => row,
        _ => {
            return Err(StorageError::NotFound {
                key: path.to_string(),
            })
        }
    };
    if let Some(want) = if_revision {
        if cur.id as u64 != want {
            return Err(StorageError::Conflict {
                key: path.to_string(),
                expected: if_revision,
                have: Some(cur.id as u64),
            });
        }
    }
    let next = read_i64(conn, schema::NEXT_REV_SQL).await?;
    conn.execute(
        schema::INSERT_ROW_SQL,
        params![
            next,
            path,
            value.to_string(),
            cur.create_revision,
            cur.id,
            0i64,
            cur.version + 1
        ],
    )
    .await
    .map_err(be)?;
    schema::meta_set(conn, schema::META_REVISION, &next.to_string()).await?;
    // The new row stores the bumped generation version explicitly.
    let entry = StoredEntry {
        key: path.to_string(),
        value: value.clone(),
        create_revision: cur.create_revision as u64,
        mod_revision: next as u64,
        version: (cur.version + 1).max(0) as u64,
    };
    Ok((entry.clone(), WatchEvent::Put(Arc::new(entry))))
}

/// Delete body (inside an open tx): appends a tombstone carrying the final
/// object, so replayed Delete events keep the last state (upstream parity).
/// `if_revision` is checked only after existence is confirmed (embedded
/// ordering).
async fn delete_tx(
    conn: &libsql::Connection,
    path: &str,
    if_revision: Option<Revision>,
) -> Result<Option<(StoredEntry, WatchEvent)>, StorageError> {
    let cur = match latest_row(conn, path).await? {
        Some(row) if !row.deleted => row,
        _ => return Ok(None),
    };
    if let Some(want) = if_revision {
        if cur.id as u64 != want {
            return Err(StorageError::Conflict {
                key: path.to_string(),
                expected: if_revision,
                have: Some(cur.id as u64),
            });
        }
    }
    let next = read_i64(conn, schema::NEXT_REV_SQL).await?;
    conn.execute(
        schema::INSERT_ROW_SQL,
        params![
            next,
            path,
            cur.value.clone(),
            cur.create_revision,
            cur.id,
            1i64,
            cur.version
        ],
    )
    .await
    .map_err(be)?;
    schema::meta_set(conn, schema::META_REVISION, &next.to_string()).await?;
    // The tombstone carries the pre-delete version unchanged, so the
    // returned entry and replayed `prev` match what `get` last returned.
    let cur_entry = schema::row_to_entry(&cur)?;
    let ev = WatchEvent::Delete {
        key: path.to_string(),
        mod_revision: next as u64,
        prev: Some(Arc::new(cur_entry.clone())),
    };
    Ok(Some((cur_entry, ev)))
}
