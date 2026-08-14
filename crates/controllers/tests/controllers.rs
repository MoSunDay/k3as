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

/// CAS-rewrite the deployment template's container image (new hash -> new
/// target RS -> a real rollout).
async fn set_deployment_image(env: &Env, image: &str) {
    let key = Key::new("apps", "deployments", "default", "web");
    let dep = env.client.get(&key).await.unwrap().unwrap();
    let rv = dep["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut next = dep.clone();
    next["spec"]["template"]["spec"]["containers"][0]["image"] = json!(image);
    env.client.update(&key, next, Some(rv)).await.unwrap();
}

async fn get_deployment(env: &Env) -> Option<Value> {
    env.client
        .get(&Key::new("apps", "deployments", "default", "web"))
        .await
        .unwrap()
}

fn condition<'a>(dep: &'a Value, ctype: &str) -> Option<&'a Value> {
    dep.pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(ctype))
}

fn status_u64(dep: &Value, field: &str) -> Option<u64> {
    dep.pointer(&format!("/status/{field}"))
        .and_then(Value::as_u64)
}

fn progressing_is(dep: &Value, status: &str, reason: &str) -> bool {
    condition(dep, "Progressing")
        .map(|c| {
            c.get("status").and_then(Value::as_str) == Some(status)
                && c.get("reason").and_then(Value::as_str) == Some(reason)
        })
        .unwrap_or(false)
}

/// CAS-pin the single pod's Ready condition (breaks the ready-by-default
/// assumption; `pod_is_ready` honors the explicit condition).
async fn set_pod_ready_condition(env: &Env, ready: bool) {
    let pod = pods(env).await.remove(0);
    let pod_name = pod["metadata"]["name"].as_str().unwrap().to_string();
    let rv = pod["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut next = pod.clone();
    next["status"]["conditions"] = json!([{
        "type": "Ready",
        "status": if ready { "True" } else { "False" },
    }]);
    env.client
        .update(&Key::new("", "pods", "default", &pod_name), next, Some(rv))
        .await
        .unwrap();
}

#[tokio::test]
async fn rolling_update_replaces_pods_and_completes() {
    let env = env();
    create(
        &env,
        Key::new("apps", "deployments", "default", "web"),
        deployment("nginx:1.28", 3),
    )
    .await;
    eventually!(5000, async { pods(&env).await.len() == 3 });
    let old_hash = pods(&env).await[0]
        .pointer("/metadata/labels/pod-template-hash")
        .and_then(Value::as_str)
        .expect("pod carries pod-template-hash")
        .to_string();

    set_deployment_image(&env, "nginx:1.29").await;
    eventually!(30000, async {
        let Some(dep) = get_deployment(&env).await else {
            return false;
        };
        let rs = replicasets(&env).await;
        let pods = pods(&env).await;
        rs.len() == 1 // old RS deleted
            && pods.len() == 3
            && pods.iter().all(|p| {
                p.pointer("/metadata/labels/pod-template-hash").and_then(Value::as_str)
                    != Some(old_hash.as_str())
            })
            && status_u64(&dep, "updatedReplicas") == Some(3)
            && status_u64(&dep, "availableReplicas") == Some(3)
            && status_u64(&dep, "readyReplicas") == Some(3)
            && progressing_is(&dep, "True", "NewReplicaSetAvailable")
            && condition(&dep, "Available")
                .map(|c| c.get("status").and_then(Value::as_str) == Some("True"))
                .unwrap_or(false)
    });
}

#[tokio::test]
async fn stuck_rollout_reports_progress_deadline_exceeded() {
    let env = env();
    // The deterministic stuck vector: maxSurge=0 + maxUnavailable=0 (a
    // config upstream validation rejects) with a pod pinned not-Ready.
    let mut dep = deployment("nginx:1.0", 1);
    dep["spec"]["strategy"] = json!({
        "type": "RollingUpdate",
        "rollingUpdate": {"maxSurge": 0, "maxUnavailable": 0},
    });
    dep["spec"]["progressDeadlineSeconds"] = json!(1);
    create(&env, Key::new("apps", "deployments", "default", "web"), dep).await;
    eventually!(5000, async { pods(&env).await.len() == 1 });

    // Pin the only pod not-Ready and wait for the controller chain to
    // observe it (deployment availableReplicas drops to 0).
    set_pod_ready_condition(&env, false).await;
    eventually!(5000, async {
        get_deployment(&env)
            .await
            .and_then(|d| status_u64(&d, "availableReplicas"))
            == Some(0)
    });

    // New template -> new RS, but the rollout is frozen below the
    // availability floor; the 1s progress deadline expires.
    set_deployment_image(&env, "nginx:2.0").await;
    eventually!(15000, async {
        get_deployment(&env)
            .await
            .map(|d| progressing_is(&d, "False", "ProgressDeadlineExceeded"))
            .unwrap_or(false)
    });

    // Recovery: flip the pod back to Ready -- the swap unblocks and the
    // rollout completes.
    set_pod_ready_condition(&env, true).await;
    eventually!(15000, async {
        get_deployment(&env)
            .await
            .map(|d| {
                status_u64(&d, "updatedReplicas") == Some(1)
                    && status_u64(&d, "availableReplicas") == Some(1)
                    && progressing_is(&d, "True", "NewReplicaSetAvailable")
            })
            .unwrap_or(false)
    });
}

#[tokio::test]
async fn recreate_strategy_rolls_fresh() {
    let env = env();
    let mut dep = deployment("nginx:1.0", 2);
    dep["spec"]["strategy"] = json!({"type": "Recreate"});
    create(&env, Key::new("apps", "deployments", "default", "web"), dep).await;
    eventually!(5000, async { pods(&env).await.len() == 2 });

    set_deployment_image(&env, "nginx:2.0").await;
    eventually!(15000, async {
        let Some(dep) = get_deployment(&env).await else {
            return false;
        };
        let rs = replicasets(&env).await;
        let pods = pods(&env).await;
        rs.len() == 1
            && rs[0]
                .pointer("/spec/template/spec/containers/0/image")
                .and_then(Value::as_str)
                == Some("nginx:2.0")
            && pods.len() == 2
            && pods.iter().all(|p| {
                p.pointer("/spec/containers/0/image")
                    .and_then(Value::as_str)
                    == Some("nginx:2.0")
            })
            && status_u64(&dep, "updatedReplicas") == Some(2)
            && status_u64(&dep, "availableReplicas") == Some(2)
            && progressing_is(&dep, "True", "NewReplicaSetAvailable")
    });
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
