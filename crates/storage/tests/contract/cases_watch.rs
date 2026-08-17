//! Shared contract cases: live watch, historical replay, and the
//! replay -> live seam (T2.2/T2.3, Q29).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use storage::{Key, KeyPrefix, Revision, StorageBackend, WatchEvent};
use tokio::time::timeout;

use super::{assert_no_event, must_recv, pod_key, pod_value};

pub(crate) async fn watch_delivers_put_and_delete_events<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let mut w = store.watch(&prefix, None).await.unwrap();
    let key = pod_key("default", "a");
    store.create(&key, pod_value("a")).await.unwrap();
    let WatchEvent::Put(e) = w.recv().await.unwrap() else {
        panic!("expected Put");
    };
    assert_eq!(e.key, "/registry/pods/default/a");
    store.delete(&key, None).await.unwrap();
    assert!(matches!(w.recv().await.unwrap(), WatchEvent::Delete { .. }));
}

pub(crate) async fn watch_filters_by_prefix<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
    let mut w = store
        .watch(&KeyPrefix::new("", "pods", None), None)
        .await
        .unwrap();
    // Create a service -- should NOT be delivered to the pods watch.
    store
        .create(
            &Key::new("", "services", "default", "s"),
            serde_json::json!({}),
        )
        .await
        .unwrap();
    store
        .create(&pod_key("default", "a"), pod_value("a"))
        .await
        .unwrap();
    let WatchEvent::Put(e) = w.recv().await.unwrap() else {
        panic!("expected Put");
    };
    assert!(e.key.contains("/pods/"));
}

pub(crate) async fn current_revision_starts_zero_and_monotonic<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
    assert_eq!(store.current_revision().await.unwrap(), 0);
    store
        .create(&pod_key("default", "a"), pod_value("a"))
        .await
        .unwrap();
    assert_eq!(store.current_revision().await.unwrap(), 1);
}

pub(crate) async fn watch_with_start_revision_replays_history_in_order<
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
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let mut w = store.watch(&prefix, Some(1)).await.unwrap();
    for (idx, name) in ["a", "b", "c"].iter().enumerate() {
        let ev = must_recv(&mut w).await;
        let WatchEvent::Put(e) = &ev else {
            panic!("expected Put for {name}");
        };
        assert!(e.key.ends_with(&format!("/{name}")), "key {}", e.key);
        assert_eq!(e.value["metadata"]["name"], *name);
        assert_eq!(ev.revision(), (idx + 1) as u64);
        assert_eq!(e.create_revision, e.mod_revision);
    }
    // Exactly 3 replayed events -- nothing else is pending.
    assert_no_event(&mut w).await;
}

pub(crate) async fn watch_replay_seam_is_lossless_and_duplicate_free<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
    for i in 1..=50 {
        let name = format!("p{i:02}");
        store
            .create(&pod_key("default", &name), pod_value(&name))
            .await
            .unwrap();
    }
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    // Subscribe BEFORE the live writes begin.
    let mut w = store.watch(&prefix, Some(1)).await.unwrap();

    let writer = Arc::clone(&store);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        for i in 1..=50 {
            let name = format!("z{i:02}");
            writer
                .create(&pod_key("default", &name), pod_value(&name))
                .await
                .unwrap();
        }
    });

    let mut revisions: Vec<u64> = Vec::with_capacity(100);
    while revisions.len() < 100 {
        match timeout(Duration::from_secs(2), w.recv()).await {
            Ok(Some(ev)) => revisions.push(ev.revision()),
            Ok(None) => break, // stream closed early: fall through to asserts
            Err(_) => panic!("timed out after {} events", revisions.len()),
        }
    }

    assert_eq!(
        revisions.len(),
        100,
        "expected exactly 100 events, got {}",
        revisions.len()
    );
    // Strictly increasing delivery order across the replay -> live seam.
    assert!(
        revisions.windows(2).all(|pair| pair[0] < pair[1]),
        "revisions not strictly increasing: {revisions:?}"
    );
    // Every revision 1..=100 exactly once: no gaps, no duplicates.
    let unique: BTreeSet<u64> = revisions.iter().copied().collect();
    assert_eq!(unique.len(), 100, "duplicate revisions: {revisions:?}");
    assert_eq!(*unique.first().unwrap(), 1);
    assert_eq!(*unique.last().unwrap(), 100);
}

pub(crate) async fn watch_replay_filters_by_prefix<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
    // Interleave pods and configmaps so revision order crosses prefixes.
    store
        .create(&pod_key("default", "p1"), pod_value("p1"))
        .await
        .unwrap();
    store
        .create(
            &Key::new("", "configmaps", "default", "c1"),
            serde_json::json!({ "data": { "k": "1" } }),
        )
        .await
        .unwrap();
    store
        .create(&pod_key("default", "p2"), pod_value("p2"))
        .await
        .unwrap();
    store
        .create(
            &Key::new("", "configmaps", "default", "c2"),
            serde_json::json!({ "data": { "k": "2" } }),
        )
        .await
        .unwrap();

    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let mut w = store.watch(&prefix, Some(1)).await.unwrap();
    for expected in ["p1", "p2"] {
        let WatchEvent::Put(e) = must_recv(&mut w).await else {
            panic!("expected Put for {expected}");
        };
        assert!(e.key.starts_with("/registry/pods/"), "key {}", e.key);
        assert!(e.key.ends_with(&format!("/{expected}")), "key {}", e.key);
    }
    // The interleaved configmap revisions (2 and 4) never leak through.
    assert_no_event(&mut w).await;
}

pub(crate) async fn watch_from_future_revision_skips_older_events<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
    store
        .create(&pod_key("default", "a"), pod_value("a"))
        .await
        .unwrap();
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let mut w = store.watch(&prefix, Some(3)).await.unwrap();

    store
        .create(&pod_key("default", "b"), pod_value("b"))
        .await
        .unwrap();
    // Revision 2 < start_revision 3: must not be delivered.
    assert_no_event(&mut w).await;

    store
        .create(&pod_key("default", "c"), pod_value("c"))
        .await
        .unwrap();
    let ev = must_recv(&mut w).await;
    let WatchEvent::Put(e) = &ev else {
        panic!("expected Put for c");
    };
    assert_eq!(ev.revision(), 3);
    assert!(e.key.ends_with("/c"));
}

pub(crate) async fn watch_without_start_revision_is_live_only<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
    store
        .create(&pod_key("default", "a"), pod_value("a"))
        .await
        .unwrap();
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let mut w = store.watch(&prefix, None).await.unwrap();

    store
        .create(&pod_key("default", "b"), pod_value("b"))
        .await
        .unwrap();
    // The pre-existing object (a, revision 1) is never replayed.
    let ev = must_recv(&mut w).await;
    let WatchEvent::Put(e) = &ev else {
        panic!("expected Put for b");
    };
    assert_eq!(ev.revision(), 2);
    assert!(
        e.key.ends_with("/b"),
        "first live event was not b: {}",
        e.key
    );
}

pub(crate) async fn delete_events_replay_final_object_and_deletion_revision<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
    let key = pod_key("default", "a");
    store.create(&key, pod_value("a")).await.unwrap();
    let v2 = serde_json::json!({ "spec": { "replicas": 2 }, "updated": true });
    store.update(&key, v2.clone(), None).await.unwrap();
    store.delete(&key, None).await.unwrap();

    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let mut w = store.watch(&prefix, Some(1)).await.unwrap();

    let ev1 = must_recv(&mut w).await;
    assert_eq!(ev1.revision(), 1);
    let WatchEvent::Put(_) = ev1 else {
        panic!("expected first Put");
    };
    let ev2 = must_recv(&mut w).await;
    assert_eq!(ev2.revision(), 2);
    let WatchEvent::Put(e2) = &ev2 else {
        panic!("expected second Put");
    };
    assert_eq!(e2.value, v2);

    // Upstream DELETED events carry the final stored object as `prev`.
    let ev3 = must_recv(&mut w).await;
    assert_eq!(ev3.revision(), 3);
    let WatchEvent::Delete {
        key: del_key,
        mod_revision,
        prev,
    } = &ev3
    else {
        panic!("expected Delete");
    };
    assert_eq!(del_key, "/registry/pods/default/a");
    assert_eq!(*mod_revision, 3);
    let prev = prev
        .as_ref()
        .expect("delete event must carry the final object");
    assert_eq!(prev.value, v2);
    assert_eq!(prev.mod_revision, 2);
    assert_no_event(&mut w).await;
}

pub(crate) async fn two_watchers_replay_independently<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
    for name in ["a", "b", "c"] {
        store
            .create(&pod_key("default", name), pod_value(name))
            .await
            .unwrap();
    }
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let mut w1 = store.watch(&prefix, Some(1)).await.unwrap();
    let mut w2 = store.watch(&prefix, Some(3)).await.unwrap();

    let mut w1_revisions = Vec::new();
    while let Some(ev) = timeout(Duration::from_millis(150), w1.recv())
        .await
        .ok()
        .flatten()
    {
        w1_revisions.push(ev.revision());
    }
    assert_eq!(w1_revisions, vec![1, 2, 3]);

    let mut w2_revisions = Vec::new();
    while let Some(ev) = timeout(Duration::from_millis(150), w2.recv())
        .await
        .ok()
        .flatten()
    {
        w2_revisions.push(ev.revision());
    }
    assert_eq!(w2_revisions, vec![3]);

    // Both continue from the same live stream afterwards.
    store
        .create(&pod_key("default", "d"), pod_value("d"))
        .await
        .unwrap();
    assert_eq!(must_recv(&mut w1).await.revision(), 4);
    assert_eq!(must_recv(&mut w2).await.revision(), 4);
}

/// Deep-review #4: a start revision beyond anything ever committed -- or
/// even representable as an i64 row id -- must behave like any start past
/// the tail: accepted (not `Compacted`, not `Backend`), replaying nothing,
/// and delivering no earlier event. A backend casting `u64 -> i64`
/// unchecked would wrap negative and synchronously replay the whole log.
pub(crate) async fn watch_at_unrepresentable_revision_replays_nothing<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
    store
        .create(&pod_key("default", "a"), pod_value("a"))
        .await
        .unwrap();
    let prefix = KeyPrefix::new("", "pods", Some("default".into()));
    let mut w = store.watch(&prefix, Some(Revision::MAX)).await.unwrap();
    // No historical replay -- an unchecked cast would deliver Put(a) here.
    assert_no_event(&mut w).await;
    // The future start guards the live seam too (min_revision): a write at
    // a far lower revision stays invisible, like any future-start watch.
    store
        .create(&pod_key("default", "b"), pod_value("b"))
        .await
        .unwrap();
    assert_no_event(&mut w).await;
    let _ = w;
}
