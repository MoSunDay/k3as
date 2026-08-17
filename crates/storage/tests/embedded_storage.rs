//! EmbeddedStorage integration tests (T2.2; T2.3/Q29 contract split).
//!
//! The 26 backend-portable semantics cases moved to `tests/contract/` and
//! are instantiated here for the embedded backend via `storage_contract!`
//! (run as `embedded::<case>`). What stays local is the one embedded-only
//! case: `history_eviction_...` pins `with_history_capacity`, a knob the
//! [`storage::StorageBackend`] trait does not expose (SQLite compaction is
//! driven by the persisted watermark instead). The same 25 cases run
//! against `SqliteStorage` in `tests/sqlite_storage.rs`.

mod contract;

use std::sync::Arc;

use contract::{assert_no_event, must_recv, pod_key, pod_value, storage_contract};
use storage::{EmbeddedStorage, KeyPrefix, StorageBackend, StorageError, WatchEvent};

storage_contract!(embedded, || async { Arc::new(EmbeddedStorage::new()) });

#[tokio::test]
async fn history_eviction_reports_compacted_and_replays_retained() {
    let store = EmbeddedStorage::with_history_capacity(2);
    for name in ["a", "b", "c", "d"] {
        store
            .create(&pod_key("default", name), pod_value(name))
            .await
            .unwrap();
    }
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let err = store
        .watch(&prefix, Some(1))
        .await
        .err()
        .expect("watch at/below the watermark must fail");
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

    let mut w = store.watch(&prefix, Some(3)).await.unwrap();
    for expected in [3u64, 4u64] {
        let ev = must_recv(&mut w).await;
        let WatchEvent::Put(e) = &ev else {
            panic!("expected Put at revision {expected}");
        };
        assert_eq!(ev.revision(), expected);
        assert!(e.create_revision <= e.mod_revision);
    }
    assert_no_event(&mut w).await;
}
