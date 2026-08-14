//! T3.1b (slice S4) acceptance: the DaemonSet controller through the real
//! `ControllerManager` (informers + workqueues + reconcilers) against
//! `EmbeddedStorage`. Nodes are created/deleted by hand here -- they are
//! the DaemonSet placement source of truth, so these tests cover both the
//! placement predicate and the node-lifecycle reaction. Every wait is a
//! poll with timeout -- NO fixed timing windows.

use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Cluster-scoped Node object (`Key::new("", "nodes", "", name)`).
fn node(name: &str, labels: Value) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {"name": name, "labels": labels},
    })
}

fn daemonset(image: &str) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "DaemonSet",
        "metadata": {"name": "agentd", "namespace": "default"},
        "spec": {
            "selector": {"matchLabels": {"app": "agentd"}},
            "template": {
                "metadata": {"labels": {"app": "agentd"}},
                "spec": {
                    "nodeSelector": {"agent": "init-pro"},
                    "containers": [{"name": "c", "image": image}],
                },
            },
        },
    })
}

async fn create_node(env: &Env, name: &str, labels: Value) {
    env.client
        .create(&Key::new("", "nodes", "", name), node(name, labels))
        .await
        .unwrap();
}

async fn delete_node(env: &Env, name: &str) {
    env.client
        .delete(&Key::new("", "nodes", "", name))
        .await
        .unwrap();
}

async fn pods(env: &Env) -> Vec<Value> {
    env.client
        .list(&KeyPrefix::new("", "pods", Some("default".into())))
        .await
        .unwrap()
}

async fn get_daemonset(env: &Env) -> Option<Value> {
    env.client
        .get(&Key::new("apps", "daemonsets", "default", "agentd"))
        .await
        .unwrap()
}

/// CAS-mutate the DaemonSet (template rewrites).
async fn update_daemonset(env: &Env, f: impl FnOnce(&mut Value)) {
    let key = Key::new("apps", "daemonsets", "default", "agentd");
    let ds = env.client.get(&key).await.unwrap().unwrap();
    let rv = ds["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut next = ds.clone();
    f(&mut next);
    env.client.update(&key, next, Some(rv)).await.unwrap();
}

fn status_u64(ds: &Value, field: &str) -> Option<u64> {
    ds.pointer(&format!("/status/{field}"))
        .and_then(Value::as_u64)
}

fn pods_on(pods: &[Value], node: &str) -> usize {
    pods.iter()
        .filter(|p| p.pointer("/spec/nodeName").and_then(Value::as_str) == Some(node))
        .count()
}

fn pod_template_hash(pod: &Value) -> Option<&str> {
    pod.pointer("/metadata/labels/pod-template-hash")
        .and_then(Value::as_str)
}

async fn converged_2(env: &Env) -> bool {
    let Some(ds) = get_daemonset(env).await else {
        return false;
    };
    let pods = pods(env).await;
    pods.len() == 2
        && pods_on(&pods, "n1") == 1
        && pods_on(&pods, "n2") == 1
        && status_u64(&ds, "desiredNumberScheduled") == Some(2)
        && status_u64(&ds, "currentNumberScheduled") == Some(2)
        && status_u64(&ds, "numberReady") == Some(2)
}

#[tokio::test]
async fn daemonset_schedules_one_pod_per_matching_node() {
    let env = env();
    create_node(&env, "n1", json!({"agent": "init-pro"})).await;
    create_node(&env, "n2", json!({"agent": "init-pro", "zone": "a"})).await;
    env.client
        .create(
            &Key::new("apps", "daemonsets", "default", "agentd"),
            daemonset("nginx:1.0"),
        )
        .await
        .unwrap();
    eventually!(10000, converged_2(&env));
}

#[tokio::test]
async fn daemonset_respects_node_selector() {
    let env = env();
    create_node(&env, "n1", json!({"agent": "init-pro"})).await;
    create_node(&env, "n2", json!({"agent": "init-pro", "zone": "a"})).await;
    // No `agent` label: never a placement target.
    create_node(&env, "n3", json!({"agent": "other", "zone": "a"})).await;
    env.client
        .create(
            &Key::new("apps", "daemonsets", "default", "agentd"),
            daemonset("nginx:1.0"),
        )
        .await
        .unwrap();
    eventually!(10000, async {
        converged_2(&env).await && pods_on(&pods(&env).await, "n3") == 0
    });
}

#[tokio::test]
async fn daemonset_reacts_to_node_lifecycle() {
    let env = env();
    create_node(&env, "n1", json!({"agent": "init-pro"})).await;
    create_node(&env, "n2", json!({"agent": "init-pro"})).await;
    env.client
        .create(
            &Key::new("apps", "daemonsets", "default", "agentd"),
            daemonset("nginx:1.0"),
        )
        .await
        .unwrap();
    eventually!(10000, converged_2(&env));

    // Node deletion: the n2 pod goes with it, desired drops to 1.
    delete_node(&env, "n2").await;
    eventually!(10000, async {
        let Some(ds) = get_daemonset(&env).await else {
            return false;
        };
        let pods = pods(&env).await;
        pods.len() == 1
            && pods_on(&pods, "n2") == 0
            && status_u64(&ds, "desiredNumberScheduled") == Some(1)
            && status_u64(&ds, "currentNumberScheduled") == Some(1)
    });

    // Node re-creation: back to one pod per matching node.
    create_node(&env, "n2", json!({"agent": "init-pro"})).await;
    eventually!(10000, converged_2(&env));
}

#[tokio::test]
async fn daemonset_rolling_update_replaces_pods() {
    let env = env();
    create_node(&env, "n1", json!({"agent": "init-pro"})).await;
    create_node(&env, "n2", json!({"agent": "init-pro"})).await;
    env.client
        .create(
            &Key::new("apps", "daemonsets", "default", "agentd"),
            daemonset("nginx:1.0"),
        )
        .await
        .unwrap();
    eventually!(10000, converged_2(&env));
    let old_hash = pods(&env)
        .await
        .first()
        .and_then(pod_template_hash)
        .expect("pod carries pod-template-hash")
        .to_string();

    // New template image: every pod is replaced at the new hash.
    update_daemonset(&env, |ds| {
        ds["spec"]["template"]["spec"]["containers"][0]["image"] = json!("nginx:2.0");
    })
    .await;
    eventually!(15000, async {
        let Some(ds) = get_daemonset(&env).await else {
            return false;
        };
        let pods = pods(&env).await;
        let desired = status_u64(&ds, "desiredNumberScheduled");
        pods.len() == 2
            && pods.iter().all(|p| {
                p.pointer("/spec/containers/0/image")
                    .and_then(Value::as_str)
                    == Some("nginx:2.0")
                    && pod_template_hash(p).map(|h| h != old_hash) == Some(true)
            })
            && status_u64(&ds, "updatedNumberScheduled") == desired
            && status_u64(&ds, "numberReady") == desired
            && status_u64(&ds, "currentNumberScheduled") == desired
    });
}
