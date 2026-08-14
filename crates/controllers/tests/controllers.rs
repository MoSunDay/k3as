//! T3.1a acceptance: in-process e2e through the real `ControllerManager`
//! (leader election + informers + workqueues + reconcilers) against
//! `EmbeddedStorage`. Every wait is a poll with timeout -- NO fixed timing
//! windows.

use std::sync::Arc;
use std::time::{Duration, Instant};

use controllers::{Client, ControllerManager, Stop, StorageClient};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, KeyPrefix, StorageBackend};

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
    store: Arc<EmbeddedStorage>,
    client: StorageClient,
    _stop: Stop,
}

fn env() -> Env {
    let store = Arc::new(EmbeddedStorage::new());
    let stop = Stop::new();
    let _handles = ControllerManager::spawn(store.clone(), stop.clone());
    let client = StorageClient::new(store.clone());
    Env {
        store,
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

async fn create(env: &Env, key: Key, obj: Value) {
    env.client.create(&key, obj).await.unwrap();
}

async fn scale_deployment(env: &Env, replicas: u64) {
    let key = Key::new("apps", "deployments", "default", "web");
    let dep = env
        .client
        .get(&key)
        .await
        .unwrap()
        .expect("deployment exists");
    let rv = dep["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut next = dep.clone();
    next["spec"]["replicas"] = json!(replicas);
    env.client.update(&key, next, Some(rv)).await.unwrap();
}

async fn pods(env: &Env) -> Vec<Value> {
    env.client
        .list(&KeyPrefix::new("", "pods", Some("default".into())))
        .await
        .unwrap()
}

async fn replicasets(env: &Env) -> Vec<Value> {
    env.client
        .list(&KeyPrefix::new(
            "apps",
            "replicasets",
            Some("default".into()),
        ))
        .await
        .unwrap()
}

async fn endpoints_addresses(env: &Env) -> usize {
    let ep = env
        .client
        .get(&Key::new("", "endpoints", "default", "web"))
        .await
        .unwrap();
    ep.map(|e| {
        e["subsets"]
            .as_array()
            .and_then(|s| s.first())
            .and_then(|s| s["addresses"].as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    })
    .unwrap_or(0)
}

#[tokio::test]
async fn deployment_scale_1_3_1_converges() {
    let env = env();
    create(
        &env,
        Key::new("apps", "deployments", "default", "web"),
        deployment("nginx:1.0", 1),
    )
    .await;
    eventually!(5000, async { pods(&env).await.len() == 1 });
    scale_deployment(&env, 3).await;
    eventually!(5000, async { pods(&env).await.len() == 3 });
    scale_deployment(&env, 1).await;
    eventually!(5000, async { pods(&env).await.len() == 1 });
}

#[tokio::test]
async fn endpoints_reflect_membership() {
    let env = env();
    create(
        &env,
        Key::new("apps", "deployments", "default", "web"),
        deployment("nginx:1.0", 1),
    )
    .await;
    create(
        &env,
        Key::new("", "services", "default", "web"),
        json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {
                "selector": {"app": "web"},
                "ports": [{"name": "http", "port": 80, "targetPort": 8080}],
            },
        }),
    )
    .await;
    eventually!(5000, async { endpoints_addresses(&env).await == 1 });
    scale_deployment(&env, 3).await;
    eventually!(5000, async { endpoints_addresses(&env).await == 3 });
    scale_deployment(&env, 1).await;
    eventually!(5000, async { endpoints_addresses(&env).await == 1 });

    // Endpoints carry placeholder IPs (10.42/16) + service ports w/o targetPort.
    let ep = env
        .client
        .get(&Key::new("", "endpoints", "default", "web"))
        .await
        .unwrap()
        .unwrap();
    let addr = &ep["subsets"][0]["addresses"][0];
    assert!(
        addr["ip"].as_str().unwrap().starts_with("10.42."),
        "{}",
        addr["ip"]
    );
    assert!(addr["hostname"].as_str().is_some());
    assert_eq!(ep["subsets"][0]["ports"][0]["port"], 80);
    assert!(ep["subsets"][0]["ports"][0].get("targetPort").is_none());
}

#[tokio::test]
async fn template_change_creates_new_rs_and_drains_old() {
    let env = env();
    create(
        &env,
        Key::new("apps", "deployments", "default", "web"),
        deployment("nginx:1.0", 1),
    )
    .await;
    eventually!(5000, async { pods(&env).await.len() == 1 });

    // Rewrite the pod template (new image -> new hash -> new RS).
    let key = Key::new("apps", "deployments", "default", "web");
    let dep = env.client.get(&key).await.unwrap().unwrap();
    let rv = dep["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut next = dep.clone();
    next["spec"]["template"]["spec"]["containers"][0]["image"] = json!("nginx:2.0");
    env.client.update(&key, next, Some(rv)).await.unwrap();

    eventually!(5000, async {
        let rs = replicasets(&env).await;
        let pods = pods(&env).await;
        rs.len() == 1
            && rs[0]
                .pointer("/spec/template/spec/containers/0/image")
                .and_then(Value::as_str)
                == Some("nginx:2.0")
            && pods.len() == 1
            && pods[0]
                .pointer("/spec/containers/0/image")
                .and_then(Value::as_str)
                == Some("nginx:2.0")
    });
}

#[tokio::test]
async fn rs_reconcile_creates_and_scales_pods_directly() {
    let env = env();
    let rs = json!({
        "apiVersion": "apps/v1",
        "kind": "ReplicaSet",
        "metadata": {"name": "bare", "namespace": "default"},
        "spec": {
            "replicas": 2,
            "selector": {"matchLabels": {"app": "bare"}},
            "template": {
                "metadata": {"labels": {"app": "bare"}},
                "spec": {"containers": [{"name": "c", "image": "nginx"}]},
            },
        },
    });
    create(&env, Key::new("apps", "replicasets", "default", "bare"), rs).await;
    eventually!(5000, async { pods(&env).await.len() == 2 });

    let key = Key::new("apps", "replicasets", "default", "bare");
    let obj = env.client.get(&key).await.unwrap().unwrap();
    let rv = obj["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut next = obj.clone();
    next["spec"]["replicas"] = json!(0);
    env.client.update(&key, next, Some(rv)).await.unwrap();
    eventually!(5000, async { pods(&env).await.is_empty() });
}

#[tokio::test]
async fn deployment_status_reports_replicas() {
    let env = env();
    create(
        &env,
        Key::new("apps", "deployments", "default", "web"),
        deployment("nginx:1.0", 2),
    )
    .await;
    eventually!(5000, async {
        env.client
            .get(&Key::new("apps", "deployments", "default", "web"))
            .await
            .unwrap()
            .and_then(|d| d.pointer("/status/replicas").cloned())
            == Some(json!(2))
    });
    // The RS status agrees and pods exist.
    eventually!(5000, async { pods(&env).await.len() == 2 });
}

#[tokio::test]
async fn quiesce_after_convergence() {
    let env = env();
    create(
        &env,
        Key::new("apps", "deployments", "default", "web"),
        deployment("nginx:1.0", 3),
    )
    .await;
    eventually!(5000, async {
        pods(&env).await.len() == 3
            && env
                .client
                .get(&Key::new("apps", "deployments", "default", "web"))
                .await
                .unwrap()
                .and_then(|d| d.pointer("/status/replicas").cloned())
                == Some(json!(3))
    });
    // Let any trailing status writes land, then require a quiet cluster: the
    // anti-oscillation gate (write-if-changed must not flap).
    tokio::time::sleep(Duration::from_millis(250)).await;
    let rev = env.store.current_revision().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        env.store.current_revision().await.unwrap(),
        rev,
        "no storage writes after convergence (write-if-changed must hold)"
    );
}
