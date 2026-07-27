//! Integration tests for EmbeddedStorage: CRUD round-trip, resourceVersion
//! monotonicity, optimistic-concurrency conflict, and watch events (T2.2).

use storage::{EmbeddedStorage, Key, KeyPrefix, StorageBackend, StorageError, WatchEvent};

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
