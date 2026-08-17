//! Shared contract cases: explicit compaction (T2.2/T2.3, Q29).

use std::sync::Arc;

use storage::{KeyPrefix, StorageBackend, StorageError, WatchEvent};

use super::{assert_no_event, must_recv, pod_key, pod_value};

pub(crate) async fn explicit_compact_returns_watermark_and_gates_watch<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
    for name in ["a", "b", "c"] {
        store
            .create(&pod_key("default", name), pod_value(name))
            .await
            .unwrap();
    }
    assert_eq!(store.compact(2).await.unwrap(), 2);

    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let err = store
        .watch(&prefix, Some(2))
        .await
        .err()
        .expect("watch at/below the watermark must fail");
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

    // Revision 3 is above the watermark: still replayable, exactly one event.
    let mut w = store.watch(&prefix, Some(3)).await.unwrap();
    let ev = must_recv(&mut w).await;
    let WatchEvent::Put(e) = &ev else {
        panic!("expected Put for c");
    };
    assert!(e.key.ends_with("/c"));
    assert_eq!(ev.revision(), 3);
    assert_no_event(&mut w).await;
}

pub(crate) async fn compact_keeps_get_and_list_intact<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
    let names = ["a", "b", "c"];
    let mut mod_revisions = Vec::new();
    for name in names {
        let created = store
            .create(&pod_key("default", name), pod_value(name))
            .await
            .unwrap();
        mod_revisions.push(created.mod_revision);
    }
    store.compact(3).await.unwrap();

    for (name, mod_rev) in names.iter().zip(&mod_revisions) {
        let got = store.get(&pod_key("default", name)).await.unwrap().unwrap();
        assert_eq!(got.mod_revision, *mod_rev);
        assert_eq!(got.value, pod_value(name));
    }
    assert_eq!(
        store
            .list(&KeyPrefix::new("", "pods", Some("default".into())))
            .await
            .unwrap()
            .len(),
        3
    );

    // Writes and live watches still work after compaction.
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let mut w = store.watch(&prefix, None).await.unwrap();
    store
        .create(&pod_key("default", "d"), pod_value("d"))
        .await
        .unwrap();
    let ev = must_recv(&mut w).await;
    let WatchEvent::Put(e) = &ev else {
        panic!("expected Put for d");
    };
    assert!(e.key.ends_with("/d"));
    assert_eq!(ev.revision(), 4);
}

pub(crate) async fn compact_clamps_future_revision_to_current<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
    store
        .create(&pod_key("default", "a"), pod_value("a"))
        .await
        .unwrap();
    // Requesting 99 with the cluster at revision 1 folds down to 1.
    assert_eq!(store.compact(99).await.unwrap(), 1);

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
                watermark: 1
            }
        ),
        "got {err:?}"
    );

    // Revision 2 is above the clamped watermark: empty replay, live delivery.
    let mut w = store.watch(&prefix, Some(2)).await.unwrap();
    assert_no_event(&mut w).await;
    store
        .create(&pod_key("default", "b"), pod_value("b"))
        .await
        .unwrap();
    let ev = must_recv(&mut w).await;
    let WatchEvent::Put(e) = &ev else {
        panic!("expected Put for b");
    };
    assert_eq!(ev.revision(), 2);
    assert!(e.key.ends_with("/b"));
}
