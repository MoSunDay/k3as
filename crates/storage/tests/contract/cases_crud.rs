//! Shared contract cases: CRUD, listing, and key layout (T2.3, Q29).
//!
//! Byte-for-byte ports of the former embedded-only tests; only the store
//! construction moved out (into the `storage_contract!` factory).

use std::sync::Arc;

use storage::{Key, KeyPrefix, StorageBackend, StorageError};

use super::{pod_key, pod_value};

pub(crate) async fn create_then_get_round_trips<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
    let key = pod_key("default", "a");
    let created = store.create(&key, pod_value("a")).await.unwrap();
    assert_eq!(created.version, 1);
    assert_eq!(created.create_revision, created.mod_revision);
    let got = store.get(&key).await.unwrap().unwrap();
    assert_eq!(got.value, pod_value("a"));
    assert_eq!(got.mod_revision, created.mod_revision);
}

pub(crate) async fn create_conflict_on_duplicate<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
    let key = pod_key("default", "a");
    store.create(&key, pod_value("a")).await.unwrap();
    let err = store.create(&key, pod_value("a")).await.unwrap_err();
    assert!(
        matches!(err, StorageError::AlreadyExists { .. }),
        "got {err:?}"
    );
}

pub(crate) async fn get_missing_returns_none<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
    assert!(store
        .get(&pod_key("default", "ghost"))
        .await
        .unwrap()
        .is_none());
}

pub(crate) async fn update_bumps_revision_and_version<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
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

pub(crate) async fn update_with_stale_revision_conflicts<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
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

pub(crate) async fn update_missing_not_found<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
    let err = store
        .update(&pod_key("default", "ghost"), serde_json::json!({}), None)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound { .. }), "got {err:?}");
}

pub(crate) async fn delete_removes_and_returns_entry<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
    let key = pod_key("default", "a");
    store.create(&key, pod_value("a")).await.unwrap();
    assert!(store.delete(&key, None).await.unwrap().is_some());
    assert!(store.get(&key).await.unwrap().is_none());
    // idempotent: second delete returns None.
    assert!(store.delete(&key, None).await.unwrap().is_none());
}

pub(crate) async fn delete_with_stale_revision_conflicts<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
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

pub(crate) async fn list_filters_by_prefix_and_namespace<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
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

pub(crate) async fn list_revision_ordered<S: StorageBackend + Send + Sync + 'static>(
    store: Arc<S>,
) {
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

pub(crate) async fn list_does_not_match_partial_resource_segment<
    S: StorageBackend + Send + Sync + 'static,
>(
    store: Arc<S>,
) {
    // "/registry/pods" must not match "/registry/podsabc".
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

pub(crate) async fn key_layout_matches_upstream_registry<
    S: StorageBackend + Send + Sync + 'static,
>(
    _store: Arc<S>,
) {
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
