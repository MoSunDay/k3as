//! SqliteStorage integration tests (T2.3, Q29).
//!
//! Two layers:
//!
//! 1. The shared backend contract (`tests/contract/`) instantiated on a
//!    fresh `:memory:` database per case (module `sqlite_memory`): the
//!    exact same 25 semantics assertions `tests/embedded_storage.rs` runs,
//!    proving etcd-faithful parity between the two backends. `:memory:`
//!    works as a per-test factory because the backend routes everything
//!    through ONE connection.
//! 2. SQLite-specific durability cases on file-backed databases (unique
//!    temp paths, WAL/SHM sidecars cleaned up): reopen preserves entries
//!    and revisions, revision allocation never regresses across restart,
//!    watch replay crosses a restart, the compaction watermark persists,
//!    and file databases run in WAL journal mode.

mod contract;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use contract::{assert_no_event, must_recv, pod_key, pod_value, storage_contract};
use storage::{KeyPrefix, SqliteStorage, StorageBackend, StorageError, WatchEvent};

storage_contract!(sqlite_memory, || async {
    Arc::new(
        SqliteStorage::open(Path::new(":memory:"))
            .await
            .expect("open :memory: sqlite store"),
    )
});

/// Unique temp db path (pid + process-local counter) for durability cases.
fn temp_db_path() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "init-pro-sqlite-test-{}-{n}.db",
        std::process::id()
    ))
}

/// Remove the db plus its WAL/SHM sidecars (best effort).
fn cleanup_db(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_string_lossy()));
    let _ = std::fs::remove_file(format!("{}-shm", path.to_string_lossy()));
}

#[tokio::test]
async fn reopen_preserves_entries_and_revisions() {
    let path = temp_db_path();
    cleanup_db(&path);
    let (a_before, b_before, revision) = {
        let store = SqliteStorage::open(&path).await.unwrap();
        let a = store
            .create(&pod_key("default", "a"), pod_value("a"))
            .await
            .unwrap();
        let b = store
            .create(&pod_key("default", "b"), pod_value("b"))
            .await
            .unwrap();
        let a2 = store
            .update(
                &pod_key("default", "a"),
                pod_value("a2"),
                Some(a.mod_revision),
            )
            .await
            .unwrap();
        let revision = store.current_revision().await.unwrap();
        assert_eq!(revision, 3);
        (a2, b, revision)
    };
    {
        let store = SqliteStorage::open(&path).await.unwrap();
        let a = store.get(&pod_key("default", "a")).await.unwrap().unwrap();
        assert_eq!(a, a_before, "entry a must survive reopen byte-identically");
        assert_eq!(a.create_revision, 1);
        assert_eq!(a.mod_revision, 3);
        assert_eq!(a.version, 2);
        let b = store.get(&pod_key("default", "b")).await.unwrap().unwrap();
        assert_eq!(b, b_before, "entry b must survive reopen byte-identically");
        assert_eq!(b.create_revision, 2);
        assert_eq!(b.mod_revision, 2);
        assert_eq!(b.version, 1);
        assert_eq!(store.current_revision().await.unwrap(), revision);
    }
    cleanup_db(&path);
}

#[tokio::test]
async fn reopen_continues_revision_monotonically() {
    let path = temp_db_path();
    cleanup_db(&path);
    let mut max_rev = 0u64;
    {
        let store = SqliteStorage::open(&path).await.unwrap();
        for name in ["a", "b", "c"] {
            let e = store
                .create(&pod_key("default", name), pod_value(name))
                .await
                .unwrap();
            assert!(e.mod_revision > max_rev);
            max_rev = e.mod_revision;
        }
    }
    let store = SqliteStorage::open(&path).await.unwrap();
    let d = store
        .create(&pod_key("default", "d"), pod_value("d"))
        .await
        .unwrap();
    // No revision reuse across restart: strictly greater than every
    // pre-restart revision.
    assert!(
        d.mod_revision > max_rev,
        "revision {d:?} reused after restart (max was {max_rev})"
    );
    assert_eq!(d.create_revision, d.mod_revision);
    assert_eq!(d.version, 1);
    assert_eq!(store.current_revision().await.unwrap(), d.mod_revision);
    cleanup_db(&path);
}

#[tokio::test]
async fn watch_replays_across_restart() {
    let path = temp_db_path();
    cleanup_db(&path);
    {
        let store = SqliteStorage::open(&path).await.unwrap();
        store
            .create(&pod_key("default", "p1"), pod_value("p1"))
            .await
            .unwrap();
        store
            .create(&pod_key("default", "p2"), pod_value("p2"))
            .await
            .unwrap();
        store.delete(&pod_key("default", "p1"), None).await.unwrap();
        assert_eq!(store.current_revision().await.unwrap(), 3);
    }
    // The kv table IS the event log, so replay crosses the restart.
    let store = SqliteStorage::open(&path).await.unwrap();
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let mut w = store.watch(&prefix, Some(2)).await.unwrap();

    // Revision 2: the live Put of p2 exactly as it was broadcast.
    let ev = must_recv(&mut w).await;
    assert_eq!(ev.revision(), 2);
    let WatchEvent::Put(e) = &ev else {
        panic!("expected Put for p2");
    };
    assert_eq!(e.key, "/registry/pods/default/p2");
    assert_eq!(e.value, pod_value("p2"));
    assert_eq!(e.version, 1);

    // Revision 3: the Delete of p1 carrying its final object as `prev`.
    let ev = must_recv(&mut w).await;
    assert_eq!(ev.revision(), 3);
    let WatchEvent::Delete {
        key,
        mod_revision,
        prev,
    } = &ev
    else {
        panic!("expected Delete for p1");
    };
    assert_eq!(key, "/registry/pods/default/p1");
    assert_eq!(*mod_revision, 3);
    let prev = prev.as_ref().expect("delete event must carry prev");
    assert_eq!(prev.value, pod_value("p1"));
    assert_eq!(prev.mod_revision, 1);

    // No gap/dup at the seam: the next write lands live at revision 4.
    store
        .create(&pod_key("default", "p3"), pod_value("p3"))
        .await
        .unwrap();
    let ev = must_recv(&mut w).await;
    assert_eq!(ev.revision(), 4);
    assert_no_event(&mut w).await;
    cleanup_db(&path);
}

#[tokio::test]
async fn compaction_watermark_survives_restart() {
    let path = temp_db_path();
    cleanup_db(&path);
    {
        let store = SqliteStorage::open(&path).await.unwrap();
        for name in ["a", "b", "c"] {
            store
                .create(&pod_key("default", name), pod_value(name))
                .await
                .unwrap();
        }
        assert_eq!(store.compact(2).await.unwrap(), 2);
    }
    let store = SqliteStorage::open(&path).await.unwrap();
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let err = store
        .watch(&prefix, Some(2))
        .await
        .err()
        .expect("watermark must survive the restart");
    assert!(
        matches!(
            err,
            StorageError::Compacted {
                requested: 2,
                watermark: 2
            }
        ),
        "got {err:?}"
    );
    // Above the persisted watermark still replays: exactly revision 3.
    let mut w = store.watch(&prefix, Some(3)).await.unwrap();
    let ev = must_recv(&mut w).await;
    assert_eq!(ev.revision(), 3);
    assert_no_event(&mut w).await;
    cleanup_db(&path);
}

#[tokio::test]
async fn wal_mode_on_file_db() {
    let path = temp_db_path();
    cleanup_db(&path);
    {
        let store = SqliteStorage::open(&path).await.unwrap();
        store
            .create(&pod_key("default", "a"), pod_value("a"))
            .await
            .unwrap();
    }
    // A second, raw handle reads the persisted journal mode.
    let db = libsql::Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn.query("PRAGMA journal_mode", ()).await.unwrap();
    let mode: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
    assert_eq!(mode, "wal");
    cleanup_db(&path);
}
