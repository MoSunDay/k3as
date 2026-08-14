//! Integration tests for EmbeddedStorage: CRUD round-trip, resourceVersion
//! monotonicity, optimistic-concurrency conflict, and watch events (T2.2).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use storage::{EmbeddedStorage, Key, KeyPrefix, StorageBackend, StorageError, WatchEvent};
use tokio::time::timeout;

fn pod_key(ns: &str, name: &str) -> Key {
    Key::new("", "pods", ns, name)
}

fn pod_value(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": name, "namespace": "default" },
        "spec": { "replicas": 1 }
    })
}

#[tokio::test]
async fn create_then_get_round_trips() {
    let store = EmbeddedStorage::new();
    let key = pod_key("default", "a");
    let created = store.create(&key, pod_value("a")).await.unwrap();
    assert_eq!(created.version, 1);
    assert_eq!(created.create_revision, created.mod_revision);
    let got = store.get(&key).await.unwrap().unwrap();
    assert_eq!(got.value, pod_value("a"));
    assert_eq!(got.mod_revision, created.mod_revision);
}

#[tokio::test]
async fn create_conflict_on_duplicate() {
    let store = EmbeddedStorage::new();
    let key = pod_key("default", "a");
    store.create(&key, pod_value("a")).await.unwrap();
    let err = store.create(&key, pod_value("a")).await.unwrap_err();
    assert!(
        matches!(err, StorageError::AlreadyExists { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn get_missing_returns_none() {
    let store = EmbeddedStorage::new();
    assert!(store
        .get(&pod_key("default", "ghost"))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn update_bumps_revision_and_version() {
    let store = EmbeddedStorage::new();
    let key = pod_key("default", "a");
    let created = store.create(&key, pod_value("a")).await.unwrap();
    let v2 = serde_json::json!({ "spec": { "replicas": 2 } });
    let updated = store
        .update(&key, v2.clone(), Some(created.mod_revision))
        .await
        .unwrap();
    assert_eq!(updated.version, 2);
    assert!(updated.mod_revision > created.mod_revision);
    assert_eq!(updated.create_revision, created.create_revision);
    assert_eq!(updated.value, v2);
}

#[tokio::test]
async fn update_with_stale_revision_conflicts() {
    let store = EmbeddedStorage::new();
    let key = pod_key("default", "a");
    let created = store.create(&key, pod_value("a")).await.unwrap();
    // First update succeeds, bumps revision.
    store
        .update(
            &key,
            serde_json::json!({ "v": 1 }),
            Some(created.mod_revision),
        )
        .await
        .unwrap();
    // Now reuse the stale revision -> conflict.
    let err = store
        .update(
            &key,
            serde_json::json!({ "v": 2 }),
            Some(created.mod_revision),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StorageError::Conflict { .. }),
        "expected conflict, got {err:?}"
    );
}

#[tokio::test]
async fn update_missing_not_found() {
    let store = EmbeddedStorage::new();
    let err = store
        .update(&pod_key("default", "ghost"), serde_json::json!({}), None)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound { .. }), "got {err:?}");
}

#[tokio::test]
async fn delete_removes_and_returns_entry() {
    let store = EmbeddedStorage::new();
    let key = pod_key("default", "a");
    store.create(&key, pod_value("a")).await.unwrap();
    assert!(store.delete(&key, None).await.unwrap().is_some());
    assert!(store.get(&key).await.unwrap().is_none());
    // idempotent: second delete returns None.
    assert!(store.delete(&key, None).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_with_stale_revision_conflicts() {
    let store = EmbeddedStorage::new();
    let key = pod_key("default", "a");
    let created = store.create(&key, pod_value("a")).await.unwrap();
    store
        .update(
            &key,
            serde_json::json!({ "v": 1 }),
            Some(created.mod_revision),
        )
        .await
        .unwrap();
    let err = store
        .delete(&key, Some(created.mod_revision))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Conflict { .. }), "got {err:?}");
}

#[tokio::test]
async fn list_filters_by_prefix_and_namespace() {
    let store = EmbeddedStorage::new();
    store
        .create(&Key::new("", "pods", "default", "a"), pod_value("a"))
        .await
        .unwrap();
    store
        .create(&Key::new("", "pods", "default", "b"), pod_value("b"))
        .await
        .unwrap();
    store
        .create(&Key::new("", "pods", "kube-system", "c"), pod_value("c"))
        .await
        .unwrap();
    store
        .create(
            &Key::new("", "services", "default", "s"),
            serde_json::json!({}),
        )
        .await
        .unwrap();

    assert_eq!(
        store
            .list(&KeyPrefix::new("", "pods", None))
            .await
            .unwrap()
            .len(),
        3
    );
    let default_pods = store
        .list(&KeyPrefix::new("", "pods", Some("default".into())))
        .await
        .unwrap();
    assert_eq!(default_pods.len(), 2);
    assert!(default_pods.iter().all(|e| e.key.contains("/default/")));
}

#[tokio::test]
async fn list_revision_ordered() {
    let store = EmbeddedStorage::new();
    store
        .create(&Key::new("", "pods", "default", "a"), pod_value("a"))
        .await
        .unwrap();
    store
        .create(&Key::new("", "pods", "default", "b"), pod_value("b"))
        .await
        .unwrap();
    store
        .create(&Key::new("", "pods", "default", "c"), pod_value("c"))
        .await
        .unwrap();
    let rvs: Vec<_> = store
        .list(&KeyPrefix::new("", "pods", None))
        .await
        .unwrap()
        .iter()
        .map(|e| e.mod_revision)
        .collect();
    assert_eq!(rvs, vec![1, 2, 3]);
}

#[tokio::test]
async fn list_does_not_match_partial_resource_segment() {
    // "/registry/pods" must not match "/registry/podsabc".
    let store = EmbeddedStorage::new();
    store
        .create(&Key::new("", "pods", "default", "a"), pod_value("a"))
        .await
        .unwrap();
    store
        .create(&Key::new("", "podsabc", "default", "z"), pod_value("z"))
        .await
        .unwrap();
    assert_eq!(
        store
            .list(&KeyPrefix::new("", "pods", None))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn key_layout_matches_upstream_registry() {
    // T2.2 acceptance: etcdctl get /registry/pods --prefix layout.
    assert_eq!(
        Key::new("", "pods", "default", "a").as_path(),
        "/registry/pods/default/a"
    );
    assert_eq!(
        Key::new("", "nodes", "", "n1").as_path(),
        "/registry/nodes/n1"
    );
    assert_eq!(
        Key::new("apps", "deployments", "default", "d").as_path(),
        "/registry/apps/deployments/default/d"
    );
    assert_eq!(
        Key::new("rbac.authorization.k8s.io", "roles", "", "r").as_path(),
        "/registry/rbac.authorization.k8s.io/roles/r"
    );
}

#[tokio::test]
async fn watch_delivers_put_and_delete_events() {
    let store = EmbeddedStorage::new();
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

#[tokio::test]
async fn watch_filters_by_prefix() {
    let store = EmbeddedStorage::new();
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

#[tokio::test]
async fn current_revision_starts_zero_and_monotonic() {
    let store = EmbeddedStorage::new();
    assert_eq!(store.current_revision().await.unwrap(), 0);
    store
        .create(&pod_key("default", "a"), pod_value("a"))
        .await
        .unwrap();
    assert_eq!(store.current_revision().await.unwrap(), 1);
}

// ---------------------------------------------------------------------------
// Watch historical replay + compaction (T2.2)
// ---------------------------------------------------------------------------

/// Recv the next event or panic; `None` (closed stream) is a failure.
async fn must_recv(w: &mut storage::Watch) -> WatchEvent {
    match timeout(Duration::from_secs(2), w.recv()).await {
        Ok(Some(ev)) => ev,
        Ok(None) => panic!("watch stream closed unexpectedly"),
        Err(_) => panic!("timed out waiting for a watch event"),
    }
}

/// Assert no further event arrives within a short window.
async fn assert_no_event(w: &mut storage::Watch) {
    assert!(
        timeout(Duration::from_millis(150), w.recv()).await.is_err(),
        "unexpected extra event delivered"
    );
}

#[tokio::test]
async fn watch_with_start_revision_replays_history_in_order() {
    let store = EmbeddedStorage::new();
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

#[tokio::test]
async fn watch_replay_seam_is_lossless_and_duplicate_free() {
    let store = Arc::new(EmbeddedStorage::new());
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

    let writer = store.clone();
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

#[tokio::test]
async fn watch_replay_filters_by_prefix() {
    let store = EmbeddedStorage::new();
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

#[tokio::test]
async fn watch_from_future_revision_skips_older_events() {
    let store = EmbeddedStorage::new();
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

#[tokio::test]
async fn watch_without_start_revision_is_live_only() {
    let store = EmbeddedStorage::new();
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

#[tokio::test]
async fn explicit_compact_returns_watermark_and_gates_watch() {
    let store = EmbeddedStorage::new();
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

#[tokio::test]
async fn compact_keeps_get_and_list_intact() {
    let store = EmbeddedStorage::new();
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

#[tokio::test]
async fn compact_clamps_future_revision_to_current() {
    let store = EmbeddedStorage::new();
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

#[tokio::test]
async fn delete_events_replay_final_object_and_deletion_revision() {
    let store = EmbeddedStorage::new();
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

#[tokio::test]
async fn two_watchers_replay_independently() {
    let store = EmbeddedStorage::new();
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
