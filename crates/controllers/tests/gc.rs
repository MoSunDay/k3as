//! T3.1b (slice S5) acceptance: garbage collection + namespace lifecycle
//! through the real `ControllerManager` (leader election + informers +
//! workqueues) against `EmbeddedStorage`. Decision **Q20**: background
//! cascade is the default (owner DELETE -> sweep), Orphan strips
//! ownerReferences, and terminating namespaces drain then finalize (the
//! terminal delete is owned here because in-process controllers bypass the
//! apiserver, Q19). Every wait is a poll with timeout -- NO fixed windows.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use controllers::{Client, ControllerManager, Stop, StorageClient};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, KeyPrefix};

/// Poll `cond` (an async block) until true, panicking after `ms`.
macro_rules! eventually {
    ($ms:expr, $cond:expr) => {{
        let deadline = Instant::now() + Duration::from_millis($ms);
        loop {
            if $cond.await {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for condition");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }};
}

/// Local copy of the controllers.rs Env helper (integration-test pattern).
struct Env {
    client: StorageClient,
    _stop: Stop,
}

fn env() -> Env {
    let store = Arc::new(EmbeddedStorage::new());
    let stop = Stop::new();
    let _handles = ControllerManager::spawn(store.clone(), stop.clone());
    let client = StorageClient::new(store.clone());
    Env {
        client,
        _stop: stop,
    }
}

fn deployment(image: &str, replicas: u64) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "replicas": replicas,
            "selector": {"matchLabels": {"app": "web"}},
            "template": {
                "metadata": {"labels": {"app": "web"}},
                "spec": {"containers": [{"name": "c", "image": image}]},
            },
        },
    })
}

async fn list(env: &Env, group: &str, resource: &str, ns: &str) -> Vec<Value> {
    env.client
        .list(&KeyPrefix::new(group, resource, Some(ns.to_string())))
        .await
        .unwrap()
}

async fn create(env: &Env, key: Key, obj: Value) {
    env.client.create(&key, obj).await.unwrap();
}

fn revision(v: &Value) -> u64 {
    v.pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn gc_cascades_deployment_delete_to_pods() {
    let env = env();
    create(
        &env,
        Key::new("apps", "deployments", "default", "web"),
        deployment("nginx:1.28", 2),
    )
    .await;
    eventually!(5000, async {
        list(&env, "", "pods", "default").await.len() == 2
    });

    // Owner delete (background cascade, Q20): RS AND pods must disappear.
    env.client
        .delete(&Key::new("apps", "deployments", "default", "web"))
        .await
        .unwrap();
    eventually!(15000, async {
        list(&env, "apps", "replicasets", "default")
            .await
            .is_empty()
            && list(&env, "", "pods", "default").await.is_empty()
    });
}

#[tokio::test]
async fn orphan_strips_owner_refs_and_keeps_replicaset() {
    let env = env();
    create(
        &env,
        Key::new("apps", "deployments", "default", "web"),
        deployment("nginx:1.28", 2),
    )
    .await;
    eventually!(5000, async {
        list(&env, "", "pods", "default").await.len() == 2
    });

    // Stamp the propagation marker (what the apiserver's
    // ?propagationPolicy=Orphan DELETE does) and delete the owner.
    let key = Key::new("apps", "deployments", "default", "web");
    let dep = env.client.get(&key).await.unwrap().unwrap();
    let mut orphaned = dep.clone();
    orphaned["metadata"]["annotations"]["init-pro.io/deletion-propagation"] = json!("Orphan");
    env.client
        .update(&key, orphaned, Some(revision(&dep)))
        .await
        .unwrap();
    env.client.delete(&key).await.unwrap();

    // The RS survives the owner delete: ownerReferences emptied, pods kept.
    eventually!(15000, async {
        let rs = list(&env, "apps", "replicasets", "default").await;
        rs.len() == 1
            && rs[0]
                .pointer("/metadata/ownerReferences")
                .and_then(Value::as_array)
                .is_some_and(|a| a.is_empty())
            && list(&env, "", "pods", "default").await.len() == 2
    });
}

#[tokio::test]
async fn dangling_owner_ref_pod_is_collected() {
    let env = env();
    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "lonely",
            "namespace": "default",
            "ownerReferences": [
                {"kind": "ReplicaSet", "name": "ghost", "uid": "", "controller": true},
            ],
        },
        "spec": {"containers": [{"name": "c", "image": "nginx"}]},
    });
    create(&env, Key::new("", "pods", "default", "lonely"), pod).await;
    eventually!(10000, async {
        list(&env, "", "pods", "default").await.is_empty()
    });
}

#[tokio::test]
async fn namespace_termination_drains_and_finalizes() {
    let env = env();
    let ns_key = Key::new("", "namespaces", "", "ns-drain");
    create(
        &env,
        ns_key.clone(),
        json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": "ns-drain", "finalizers": ["kubernetes"]},
        }),
    )
    .await;
    create(
        &env,
        Key::new("", "configmaps", "ns-drain", "keepme"),
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "keepme", "namespace": "ns-drain"},
            "data": {"k": "v"},
        }),
    )
    .await;
    let mut dep = deployment("nginx:1.28", 1);
    dep["metadata"]["namespace"] = json!("ns-drain");
    create(
        &env,
        Key::new("apps", "deployments", "ns-drain", "web"),
        dep,
    )
    .await;
    eventually!(5000, async {
        list(&env, "", "pods", "ns-drain").await.len() == 1
    });

    // Simulate the apiserver soft delete: stamp deletionTimestamp (CAS).
    let ns = env.client.get(&ns_key).await.unwrap().unwrap();
    let mut terminating = ns.clone();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    terminating["metadata"]["deletionTimestamp"] = json!(controllers::time::now_rfc3339(now));
    env.client
        .update(&ns_key, terminating, Some(revision(&ns)))
        .await
        .unwrap();

    // Drain -> finalize: every namespaced object AND the Namespace object
    // itself must disappear (the terminal delete, Q19/Q20).
    eventually!(20000, async {
        list(&env, "", "configmaps", "ns-drain").await.is_empty()
            && list(&env, "apps", "deployments", "ns-drain")
                .await
                .is_empty()
            && list(&env, "apps", "replicasets", "ns-drain")
                .await
                .is_empty()
            && list(&env, "", "pods", "ns-drain").await.is_empty()
            && env.client.get(&ns_key).await.unwrap().is_none()
    });
}
