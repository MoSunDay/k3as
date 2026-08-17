//! Unit smoke tests for [`SqliteStorage`] (T2.3 S0-S3): CRUD round-trip +
//! conflict paths on `:memory:`, file-backed reopen durability, and watch
//! replay/compaction. The full cross-backend contract suite lives in
//! `tests/contract/` (instantiated for both backends); these stay as the
//! backend's own fast in-module gate.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::SqliteStorage;
use crate::backend::StorageBackend;
use crate::entry::WatchEvent;
use crate::error::StorageError;
use crate::key::{Key, KeyPrefix};

fn pod_key(name: &str) -> Key {
    Key::new("", "pods", "default", name)
}

fn pod_prefix() -> KeyPrefix {
    KeyPrefix::new("", "pods", Some("default".to_string()))
}

fn pod_value(name: &str) -> serde_json::Value {
    serde_json::json!({ "metadata": { "name": name } })
}

/// Unique temp db path (pid + process-local counter), cleaned up by the
/// caller together with its WAL/SHM sidecars.
fn temp_db_path() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "init-pro-storage-t223-{}-{n}.db",
        std::process::id()
    ))
}

fn cleanup_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_string_lossy()));
    let _ = std::fs::remove_file(format!("{}-shm", path.to_string_lossy()));
}

#[tokio::test]
async fn memory_crud_roundtrip_and_error_paths() {
    let store = SqliteStorage::open(std::path::Path::new(":memory:"))
        .await
        .unwrap();

    // Create + get round-trip.
    let key = pod_key("a");
    let created = store.create(&key, pod_value("a")).await.unwrap();
    assert_eq!(created.version, 1);
    assert_eq!(created.create_revision, created.mod_revision);
    let got = store.get(&key).await.unwrap().unwrap();
    assert_eq!(got.value, pod_value("a"));
    assert_eq!(got.create_revision, created.create_revision);
    assert_eq!(got.mod_revision, created.mod_revision);
    assert_eq!(got.version, 1);

    // Duplicate create -> AlreadyExists.
    let err = store.create(&key, pod_value("a")).await.unwrap_err();
    assert!(
        matches!(err, StorageError::AlreadyExists { .. }),
        "got {err:?}"
    );

    // Update with a stale revision -> Conflict.
    let err = store
        .update(&key, pod_value("a2"), Some(created.mod_revision + 1))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Conflict { .. }), "got {err:?}");

    // Update with the live revision -> version + mod_revision bump.
    let updated = store
        .update(&key, pod_value("a2"), Some(created.mod_revision))
        .await
        .unwrap();
    assert_eq!(updated.version, 2);
    assert_eq!(updated.create_revision, created.create_revision);
    assert!(updated.mod_revision > created.mod_revision);

    // Delete then get -> None (and delete again is a no-op).
    let removed = store.delete(&key, None).await.unwrap().unwrap();
    assert_eq!(removed.mod_revision, updated.mod_revision);
    assert!(store.get(&key).await.unwrap().is_none());
    assert!(store.delete(&key, None).await.unwrap().is_none());

    // Update after delete -> NotFound.
    let err = store.update(&key, pod_value("a3"), None).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound { .. }), "got {err:?}");
}

#[tokio::test]
async fn list_orders_by_mod_revision_and_scopes_prefix() {
    let store = SqliteStorage::open(std::path::Path::new(":memory:"))
        .await
        .unwrap();
    store.create(&pod_key("b"), pod_value("b")).await.unwrap();
    store.create(&pod_key("a"), pod_value("a")).await.unwrap();
    store.create(&pod_key("c"), pod_value("c")).await.unwrap();
    // A different collection must not leak into the pods list.
    let svc = Key::new("", "services", "default", "s");
    store.create(&svc, pod_value("s")).await.unwrap();

    let listed = store.list(&pod_prefix()).await.unwrap();
    let names: Vec<&str> = listed
        .iter()
        .map(|e| e.value["metadata"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["b", "a", "c"]);
    assert!(listed
        .windows(2)
        .all(|w| w[0].mod_revision < w[1].mod_revision));
}

#[tokio::test]
async fn file_backed_reopen_preserves_entries_and_revisions() {
    let path = temp_db_path();
    cleanup_db(&path);
    {
        let store = SqliteStorage::open(&path).await.unwrap();
        let a = store.create(&pod_key("a"), pod_value("a")).await.unwrap();
        store.create(&pod_key("b"), pod_value("b")).await.unwrap();
        let a2 = store
            .update(&pod_key("a"), pod_value("a2"), Some(a.mod_revision))
            .await
            .unwrap();
        assert_eq!(a2.version, 2);
        assert_eq!(store.current_revision().await.unwrap(), 3);
    }
    {
        let store = SqliteStorage::open(&path).await.unwrap();
        let got = store.get(&pod_key("a")).await.unwrap().unwrap();
        assert_eq!(got.value, pod_value("a2"));
        assert_eq!(got.create_revision, 1);
        assert_eq!(got.mod_revision, 3);
        assert_eq!(got.version, 2);
        let b = store.get(&pod_key("b")).await.unwrap().unwrap();
        assert_eq!(b.create_revision, 2);
        assert_eq!(b.mod_revision, 2);
        assert_eq!(store.current_revision().await.unwrap(), 3);
        // The next write must allocate a strictly greater revision.
        let c = store.create(&pod_key("c"), pod_value("c")).await.unwrap();
        assert!(c.mod_revision > 3);
    }
    cleanup_db(&path);
}

#[tokio::test]
async fn file_db_runs_in_wal_mode() {
    let path = temp_db_path();
    cleanup_db(&path);
    {
        let store = SqliteStorage::open(&path).await.unwrap();
        store.create(&pod_key("a"), pod_value("a")).await.unwrap();
    }
    // A second, raw handle reads the persisted journal mode.
    let db = libsql::Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn.query("PRAGMA journal_mode", ()).await.unwrap();
    let mode: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
    assert_eq!(mode, "wal");
    cleanup_db(&path);
}

#[tokio::test]
async fn watch_replays_history_then_streams_live() {
    let store = SqliteStorage::open(std::path::Path::new(":memory:"))
        .await
        .unwrap();
    store.create(&pod_key("a"), pod_value("a")).await.unwrap();
    store.create(&pod_key("b"), pod_value("b")).await.unwrap();
    store.create(&pod_key("c"), pod_value("c")).await.unwrap();

    // Replay from revision 2 (inclusive): events at 2 and 3 ...
    let mut w = store.watch(&pod_prefix(), Some(2)).await.unwrap();
    let mut revs = Vec::new();
    for _ in 0..2 {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), w.recv())
            .await
            .expect("replay event")
            .expect("open stream");
        revs.push(ev.revision());
    }
    assert_eq!(revs, vec![2, 3]);

    // ... then the live event at revision 4.
    store.create(&pod_key("d"), pod_value("d")).await.unwrap();
    let ev = tokio::time::timeout(std::time::Duration::from_secs(5), w.recv())
        .await
        .expect("live event")
        .expect("open stream");
    assert_eq!(ev.revision(), 4);
    assert!(matches!(ev, WatchEvent::Put(_)));
}

#[tokio::test]
async fn compact_raises_watermark_and_rejects_old_watch() {
    let store = SqliteStorage::open(std::path::Path::new(":memory:"))
        .await
        .unwrap();
    for name in ["a", "b", "c"] {
        store.create(&pod_key(name), pod_value(name)).await.unwrap();
    }
    // Revision 4 supersedes revision 1 (same key): compact(2) may drop it.
    store
        .update(&pod_key("a"), pod_value("a2"), Some(1))
        .await
        .unwrap();
    assert_eq!(store.current_revision().await.unwrap(), 4);

    let watermark = store.compact(2).await.unwrap();
    assert_eq!(watermark, 2);

    // Watch at or below the watermark -> Compacted (etcd 410 Gone parity).
    let err = store
        .watch(&pod_prefix(), Some(1))
        .await
        .err()
        .expect("compacted");
    assert!(
        matches!(
            err,
            StorageError::Compacted {
                requested: 1,
                watermark: 2
            }
        ),
        "got {err:?}"
    );
    let err = store
        .watch(&pod_prefix(), Some(2))
        .await
        .err()
        .expect("compacted");
    assert!(matches!(err, StorageError::Compacted { .. }), "got {err:?}");

    // Above the watermark still replays; get/list survive compaction.
    let mut w = store.watch(&pod_prefix(), Some(3)).await.unwrap();
    let mut revs = Vec::new();
    for _ in 0..2 {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), w.recv())
            .await
            .expect("replay event")
            .expect("open stream");
        revs.push(ev.revision());
    }
    assert_eq!(revs, vec![3, 4]);
    assert_eq!(store.list(&pod_prefix()).await.unwrap().len(), 3);
    let a = store.get(&pod_key("a")).await.unwrap().unwrap();
    assert_eq!(a.mod_revision, 4);
    assert_eq!(a.value, pod_value("a2"));
    // The explicit `version` column survives compaction (S3 fix): dropping
    // the superseded row must not shrink the stored generation version,
    // matching EmbeddedStorage exactly.
    assert_eq!(a.version, 2);

    // Watermark never regresses.
    assert_eq!(store.compact(1).await.unwrap(), 2);
}

// Deep-review hardening (#2/#3): a write future dropped (aborted) between
// BEGIN and COMMIT -- or a COMMIT that itself failed -- leaves the
// transaction open on the single shared connection once the mutex guard is
// released. The next write (and compaction) must self-heal via ROLLBACK
// instead of failing until process restart.
#[tokio::test]
async fn leftover_open_write_tx_self_heals_on_next_write_and_compact() {
    let store = SqliteStorage::open(std::path::Path::new(":memory:"))
        .await
        .unwrap();
    store.create(&pod_key("a"), pod_value("a")).await.unwrap();

    // The exact state a dropped/failed write leaves behind: transaction
    // open on the shared connection, mutex released.
    {
        let conn = store.conn.lock().await;
        conn.execute("BEGIN IMMEDIATE", ()).await.unwrap();
        assert!(!conn.is_autocommit());
    }

    let b = store.create(&pod_key("b"), pod_value("b")).await.unwrap();
    assert_eq!(b.mod_revision, 2, "healed write must not reuse a revision");
    assert!(store.get(&pod_key("b")).await.unwrap().is_some());

    // The compaction path opens a write transaction too and heals likewise.
    {
        let conn = store.conn.lock().await;
        conn.execute("BEGIN IMMEDIATE", ()).await.unwrap();
        assert!(!conn.is_autocommit());
    }
    assert_eq!(store.compact(2).await.unwrap(), 2);

    let c = store.create(&pod_key("c"), pod_value("c")).await.unwrap();
    assert_eq!(c.mod_revision, 3);
}

// Deep-review hardening (#2): a COMMIT that itself fails must surface the
// error AND roll back so the connection stays usable. COMMIT with no open
// transaction is a genuine, deterministic COMMIT failure ("cannot commit -
// no transaction is active").
#[tokio::test]
async fn failed_commit_surfaces_error_and_keeps_connection_usable() {
    let store = SqliteStorage::open(std::path::Path::new(":memory:"))
        .await
        .unwrap();
    store.create(&pod_key("a"), pod_value("a")).await.unwrap();

    let (r, autocommit) = {
        let conn = store.conn.lock().await;
        let r: Result<(), StorageError> = super::finish_tx(&conn, Ok(())).await;
        (r, conn.is_autocommit())
    };
    assert!(
        matches!(r, Err(StorageError::Backend(_))),
        "expected COMMIT failure, got {r:?}"
    );
    assert!(autocommit, "failed COMMIT must not leave an open tx");

    let b = store.create(&pod_key("b"), pod_value("b")).await.unwrap();
    assert_eq!(b.mod_revision, 2);
}
