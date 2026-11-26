//! RocksDB-backed compaction cases for [`super::RdbStorage`] (T2.3/Q33, B3).
//!
//! In-crate (not `tests/`) because they assert on the PHYSICAL `log/` prefix
//! -- the thing that grows forever without compaction -- and the key layout
//! is private to this module. Every case gets its own data directory
//! (RocksDB holds a directory lock) and removes it afterwards.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rdb::store::ops;
use tokio::time::timeout;

use super::keys;
use super::scan;
use crate::backend::{StorageBackend, Watch};
use crate::entry::{Revision, WatchEvent};
use crate::error::StorageError;
use crate::key::{Key, KeyPrefix};
use crate::rdb_backend::{compaction_plan, RdbStorage, DEFAULT_RETAIN};

/// Fresh data directory per case (pid + process-local counter).
fn temp_data_dir() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "init-pro-rdb-compact-test-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_dir_all(path);
}

fn pod_key(name: &str) -> Key {
    Key::new("", "pods", "default", name)
}

fn pod_value(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": name, "namespace": "default" },
    })
}

async fn put_pods(store: &RdbStorage, names: &[&str]) {
    for name in names {
        store
            .create(&pod_key(name), pod_value(name))
            .await
            .expect("create");
    }
}

fn pods_prefix() -> KeyPrefix {
    KeyPrefix::new("", "pods", Some("default".into()))
}

/// Every revision still physically retained under `log/`, ascending.
fn retained_log_revisions(store: &RdbStorage) -> Vec<Revision> {
    let mut out: Vec<Revision> = Vec::new();
    let scan = ops::for_each_from(&store.db, keys::LOG_PREFIX, false, &mut |k, _| {
        if !k.starts_with(keys::LOG_PREFIX) {
            return false; // ran past the log range
        }
        if let Some(rev) = keys::log_revision(k) {
            out.push(rev);
        }
        true
    });
    scan.expect("scan log/");
    out.sort_unstable();
    out
}

/// Drain one event or panic (a replay that stalls is a hanging test).
async fn must_recv(watch: &mut Watch) -> WatchEvent {
    timeout(Duration::from_secs(5), watch.recv())
        .await
        .expect("event within the budget")
        .expect("the stream stayed open")
}

#[tokio::test]
async fn compact_retaining_drops_old_log_records_and_keeps_recent_ones() {
    let path = temp_data_dir();
    let store = RdbStorage::open(&path).await.expect("open");
    put_pods(&store, &["a", "b", "c", "d", "e", "f", "g", "h"]).await;
    assert_eq!(store.revision().await.unwrap(), 8);
    assert_eq!(retained_log_revisions(&store), (1..=8).collect::<Vec<_>>());

    // Keep the latest 3 revisions (6, 7, 8): the watermark is 5.
    assert_eq!(compaction_plan(8, 3), Some(5));
    assert_eq!(store.compact_retaining(3).await.unwrap(), 5);

    // The physical proof: `log/1..=log/5` are gone, `log/6..=log/8` stay.
    assert_eq!(retained_log_revisions(&store), vec![6, 7, 8]);

    // Reads are unaffected: every object still resolves from `obj/`.
    for name in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        let got = store.get(&pod_key(name)).await.unwrap().expect("readable");
        assert_eq!(got.value, pod_value(name));
    }
    assert_eq!(store.list(&pods_prefix()).await.unwrap().len(), 8);
    drop(store);
    cleanup(&path);
}

#[tokio::test]
async fn watch_below_the_watermark_is_compacted_and_above_it_replays() {
    let path = temp_data_dir();
    let store = RdbStorage::open(&path).await.expect("open");
    put_pods(&store, &["a", "b", "c", "d", "e", "f", "g", "h"]).await;
    let watermark = store.compact_retaining(3).await.unwrap();
    assert_eq!(watermark, 5);

    // 410-style refusal at AND below the watermark: the client re-LISTs.
    for start in [1u64, watermark] {
        let err = store
            .watch(&pods_prefix(), Some(start))
            .await
            .err()
            .expect("a compacted start must fail");
        assert!(
            matches!(
                err,
                StorageError::Compacted {
                    requested,
                    watermark: w
                } if requested == start && w == watermark
            ),
            "start {start}: got {err:?}"
        );
    }

    // Above the watermark the retained window replays in revision order.
    let mut w = store.watch(&pods_prefix(), Some(6)).await.unwrap();
    for (i, name) in ["f", "g", "h"].iter().enumerate() {
        let ev = must_recv(&mut w).await;
        assert_eq!(ev.revision(), 6 + i as u64);
        let WatchEvent::Put(e) = ev else {
            panic!("expected Put for {name}");
        };
        assert!(e.key.ends_with(&format!("/{name}")), "got {}", e.key);
    }
    assert!(timeout(Duration::from_millis(100), w.recv()).await.is_err());
    drop(store);
    cleanup(&path);
}

#[tokio::test]
async fn compact_retaining_is_a_no_op_while_history_is_short() {
    let path = temp_data_dir();
    let store = RdbStorage::open(&path).await.expect("open");
    put_pods(&store, &["a", "b", "c"]).await;
    assert_eq!(compaction_plan(3, DEFAULT_RETAIN), None);

    // The standing watermark is reported and nothing is dropped: a watcher
    // can still replay from revision 1.
    assert_eq!(store.compact_retaining(DEFAULT_RETAIN).await.unwrap(), 0);
    assert_eq!(retained_log_revisions(&store), vec![1, 2, 3]);
    let mut w = store.watch(&pods_prefix(), Some(1)).await.unwrap();
    for rev in 1..=3u64 {
        assert_eq!(must_recv(&mut w).await.revision(), rev);
    }
    drop(store);
    cleanup(&path);
}

#[tokio::test]
async fn compact_retaining_is_monotonic_across_calls() {
    let path = temp_data_dir();
    let store = RdbStorage::open(&path).await.expect("open");
    put_pods(&store, &["a", "b", "c", "d", "e"]).await;
    assert_eq!(store.compact_retaining(2).await.unwrap(), 3);
    // A second, more conservative pass must never lower the watermark.
    assert_eq!(store.compact_retaining(4).await.unwrap(), 3);
    put_pods(&store, &["f", "g", "h"]).await;
    assert_eq!(store.compact_retaining(4).await.unwrap(), 4);
    assert_eq!(retained_log_revisions(&store), vec![5, 6, 7, 8]);
    drop(store);
    cleanup(&path);
}

#[tokio::test]
async fn revision_helper_matches_the_trait() {
    let path = temp_data_dir();
    let store = Arc::new(RdbStorage::open(&path).await.expect("open"));
    assert_eq!(store.revision().await.unwrap(), 0);
    put_pods(&store, &["a"]).await;
    assert_eq!(store.revision().await.unwrap(), 1);
    assert_eq!(
        StorageBackend::current_revision(store.as_ref())
            .await
            .unwrap(),
        1
    );
    drop(store);
    cleanup(&path);
}

/// T2.3 hardening: the `obj/` prefix scan (blocking-pool twin of `list`)
/// is RV-ordered and honors the namespace segment delimiter --
/// `/pods/default/` never matches a sibling namespace's `/pods/default-x/`.
/// Runs on a current-thread runtime, which is exactly where a
/// `block_in_place` hop would have panicked.
#[tokio::test]
async fn list_scan_is_rv_ordered_and_namespace_delimited() {
    let path = temp_data_dir();
    let store = RdbStorage::open(&path).await.expect("open");
    // Created out of name order on purpose: LIST order is by mod_revision.
    put_pods(&store, &["c", "a", "b"]).await;
    let sibling = Key::new("", "pods", "default-x", "z");
    store
        .create(&sibling, pod_value("z"))
        .await
        .expect("create");
    let lo = keys::obj_list_prefix(&format!("{}/", pods_prefix().as_path()));
    let names: Vec<String> = scan::scan_entries(&store.db, &lo)
        .expect("scan")
        .into_iter()
        .map(|e| e.value["metadata"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["c", "a", "b"]);
    // The trait path (blocking pool) agrees with the pure scan.
    let listed = store.list(&pods_prefix()).await.unwrap();
    assert_eq!(listed.len(), 3, "the sibling namespace stays out");
    assert_eq!(listed[0].mod_revision, 1);
    drop(store);
    cleanup(&path);
}

/// T2.3 hardening: the `log/` replay scan (blocking-pool twin of the watch
/// replay) is inclusive at the start revision, revision-ordered and
/// filtered by prefix -- the synchronous scan's contract, unchanged.
#[tokio::test]
async fn replay_scan_is_inclusive_ordered_and_prefix_filtered() {
    let path = temp_data_dir();
    let store = RdbStorage::open(&path).await.expect("open");
    put_pods(&store, &["a", "b", "c"]).await;
    let watch_path = format!("{}/", pods_prefix().as_path());
    let evs = scan::scan_replay(&store.db, &keys::log_key(2), &watch_path).expect("scan");
    assert_eq!(evs.len(), 2, "revisions 2 and 3 only (start is inclusive)");
    assert!(matches!(evs[0], WatchEvent::Put(ref e) if e.mod_revision == 2));
    assert!(matches!(evs[1], WatchEvent::Put(ref e) if e.mod_revision == 3));
    let none =
        scan::scan_replay(&store.db, &keys::log_key(2), "/registry/services/").expect("scan");
    assert!(none.is_empty(), "another resource prefix replays nothing");
    drop(store);
    cleanup(&path);
}
