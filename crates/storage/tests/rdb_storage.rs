//! RdbStorage integration tests (T2.3/Q33).
//!
//! Two layers:
//!
//! 1. The shared backend contract (`tests/contract/`) instantiated on a
//!    fresh RocksDB data directory per case (module `rdb`): the exact same
//!    26 semantics assertions the embedded and sqlite_memory suites run,
//!    proving etcd-faithful parity across all three backends. RocksDB takes
//!    a directory lock, so paths are NEVER reused -- every case gets its own
//!    `temp_dir()/init-pro-rdb-test-<pid>-<n>` directory (the pid + counter
//!    pattern of the crate's other tests).
//! 2. rdb-specific durability cases (reopen / monotonic revision / watch
//!    reopen preserves entries + revisions, revision allocation never
//!    regresses across restart, watch replay crosses a restart, and the
//!    compaction watermark persists. Each scopes the first handle before
//!    reopening so the directory lock is released.

mod contract;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use contract::{assert_no_event, must_recv, pod_key, pod_value, storage_contract};
use storage::{KeyPrefix, RdbStorage, StorageBackend, StorageError, WatchEvent};

storage_contract!(rdb, || async {
    Arc::new(
        RdbStorage::open(&temp_data_dir())
            .await
            .expect("open rocksdb store"),
    )
});

/// Unique temp data directory (pid + process-local counter). The counter
/// makes paths unique within a run; the pid makes them unique across
/// concurrent runs. A leftover from a crashed earlier run is removed first
/// (best effort) -- RocksDB re-creates the directory contents.
fn temp_data_dir() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("init-pro-rdb-test-{}-{n}", std::process::id()));
    cleanup_dir(&path);
    path
}

/// Remove a data directory (best effort).
fn cleanup_dir(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

#[tokio::test]
async fn reopen_preserves_entries_and_revisions() {
    let path = temp_data_dir();
    let (a_before, b_before, revision) = {
        let store = RdbStorage::open(&path).await.unwrap();
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
        let store = RdbStorage::open(&path).await.unwrap();
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
    cleanup_dir(&path);
}

#[tokio::test]
async fn reopen_continues_revision_monotonically() {
    let path = temp_data_dir();
    let mut max_rev = 0u64;
    {
        let store = RdbStorage::open(&path).await.unwrap();
        for name in ["a", "b", "c"] {
            let e = store
                .create(&pod_key("default", name), pod_value(name))
                .await
                .unwrap();
            assert!(e.mod_revision > max_rev);
            max_rev = e.mod_revision;
        }
    }
    let store = RdbStorage::open(&path).await.unwrap();
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
    cleanup_dir(&path);
}

#[tokio::test]
async fn watch_replays_across_restart() {
    let path = temp_data_dir();
    {
        let store = RdbStorage::open(&path).await.unwrap();
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
    // The persisted log/ records ARE the event log, so replay crosses the
    // restart.
    let store = RdbStorage::open(&path).await.unwrap();
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

    assert_no_event(&mut w).await;
    cleanup_dir(&path);
}

#[tokio::test]
async fn compaction_watermark_survives_restart() {
    let path = temp_data_dir();
    {
        let store = RdbStorage::open(&path).await.unwrap();
        for name in ["a", "b", "c"] {
            store
                .create(&pod_key("default", name), pod_value(name))
                .await
                .unwrap();
        }
        assert_eq!(store.compact(2).await.unwrap(), 2);
    }
    let store = RdbStorage::open(&path).await.unwrap();
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
    cleanup_dir(&path);
}

#[tokio::test]
async fn double_open_fails_cleanly_while_locked() {
    // RocksDB holds a directory lock; the second open must surface as a
    // StorageError (Backend), never a panic -- the DSN dispatcher treats any
    // open failure as a fatal-but-clean boot error.
    let path = temp_data_dir();
    let _store = RdbStorage::open(&path).await.unwrap();
    // (No `Debug` on the backend -- sqlite parity -- so match instead of `expect_err`.)
    let err = match RdbStorage::open(&path).await {
        Err(e) => e,
        Ok(_) => panic!("second open must fail while the first handle is alive"),
    };
    assert!(matches!(err, StorageError::Backend(_)), "got {err:?}");
}
