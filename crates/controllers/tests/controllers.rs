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

// --- StatefulSet (T3.1b slice S3) -----------------------------------------

fn statefulset(replicas: u64) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "replicas": replicas,
            "serviceName": "web-svc",
            "selector": {"matchLabels": {"app": "web"}},
            "template": {
                "metadata": {"labels": {"app": "web"}},
                "spec": {"containers": [{"name": "c", "image": "nginx:1.28"}]},
            },
            "volumeClaimTemplates": [{
                "metadata": {"name": "data"},
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "resources": {"requests": {"storage": "1Gi"}},
                },
            }],
        },
    })
}

async fn get_statefulset(env: &Env) -> Option<Value> {
    env.client
        .get(&Key::new("apps", "statefulsets", "default", "web"))
        .await
        .unwrap()
}

/// CAS-mutate the StatefulSet (scale / template / strategy rewrites).
async fn update_statefulset(env: &Env, f: impl FnOnce(&mut Value)) {
    let key = Key::new("apps", "statefulsets", "default", "web");
    let sts = env.client.get(&key).await.unwrap().unwrap();
    let rv = sts["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut next = sts.clone();
    f(&mut next);
    env.client.update(&key, next, Some(rv)).await.unwrap();
}

async fn named_pod(env: &Env, name: &str) -> Option<Value> {
    env.client
        .get(&Key::new("", "pods", "default", name))
        .await
        .unwrap()
}

async fn pvcs(env: &Env) -> Vec<Value> {
    env.client
        .list(&KeyPrefix::new(
            "",
            "persistentvolumeclaims",
            Some("default".into()),
        ))
        .await
        .unwrap()
}

async fn revisions(env: &Env) -> Vec<Value> {
    env.client
        .list(&KeyPrefix::new(
            "apps",
            "controllerrevisions",
            Some("default".into()),
        ))
        .await
        .unwrap()
}

fn names_of(objs: &[Value]) -> Vec<&str> {
    objs.iter()
        .filter_map(|o| o["metadata"]["name"].as_str())
        .collect()
}

fn pod_revision_hash(pod: &Value) -> Option<&str> {
    pod.pointer("/metadata/labels/controller-revision-hash")
        .and_then(Value::as_str)
}

#[tokio::test]
async fn statefulset_ordered_scale_up_creates_named_pods_and_pvcs() {
    let env = env();
    create(
        &env,
        Key::new("apps", "statefulsets", "default", "web"),
        statefulset(3),
    )
    .await;
    eventually!(30000, async {
        let pods = pods(&env).await;
        if names_of(&pods) != ["web-0", "web-1", "web-2"] {
            return false;
        }
        let pvcs = pvcs(&env).await;
        if names_of(&pvcs) != ["data-web-0", "data-web-1", "data-web-2"] {
            return false;
        }
        // The PVC carries the claim template spec + the STS controller ref.
        let pvc = &pvcs[0];
        pvc.pointer("/spec/resources/requests/storage").and_then(Value::as_str)
            == Some("1Gi")
            && pvc.pointer("/metadata/ownerReferences/0/kind").and_then(Value::as_str)
                == Some("StatefulSet")
            // Status: fully converged at one revision.
            && get_statefulset(&env)
                .await
                .and_then(|s| s.pointer("/status").cloned())
                .map(|st| {
                    st["replicas"] == json!(3)
                        && st["readyReplicas"] == json!(3)
                        && st["availableReplicas"] == json!(3)
                        && st["updatedReplicas"] == json!(3)
                        && st["currentRevision"] == st["updateRevision"]
                        && st["currentRevision"].as_str().map(|r| r.starts_with("web-"))
                            == Some(true)
                })
                == Some(true)
            // Revision history: one ControllerRevision, revision >= 1.
            && revisions(&env).await.iter().any(|rev| {
                rev.pointer("/revision").and_then(Value::as_u64) >= Some(1)
                    && rev.pointer("/metadata/ownerReferences/0/name").and_then(Value::as_str)
                        == Some("web")
            })
    });
    // Stable pod identity: hostname/subdomain land in the pod spec.
    let web0 = named_pod(&env, "web-0").await.unwrap();
    assert_eq!(web0["spec"]["hostname"], "web-0");
    assert_eq!(web0["spec"]["subdomain"], "web-svc");
}

#[tokio::test]
async fn statefulset_scale_down_keeps_pvcs_and_deletes_high_ordinals_first() {
    let env = env();
    create(
        &env,
        Key::new("apps", "statefulsets", "default", "web"),
        statefulset(3),
    )
    .await;
    eventually!(15000, async { pods(&env).await.len() == 3 });

    update_statefulset(&env, |s| s["spec"]["replicas"] = json!(1)).await;
    eventually!(15000, async {
        let pods = pods(&env).await;
        pods.len() == 1
            && pods[0]["metadata"]["name"] == "web-0"
            && get_statefulset(&env)
                .await
                .and_then(|s| s.pointer("/status/replicas").cloned())
                == Some(json!(1))
    });
    // Retention: every claim from the scaled-down ordinals survives.
    let binding = pvcs(&env).await;
    let mut kept = names_of(&binding);
    kept.sort_unstable();
    assert_eq!(kept, ["data-web-0", "data-web-1", "data-web-2"]);
}

/// CAS-pin one pod's Ready condition (pod_is_ready honors the explicit
/// condition, breaking the ready-by-default assumption).
async fn set_named_pod_ready(env: &Env, pod_name: &str, ready: bool) {
    let pod = named_pod(env, pod_name).await.unwrap();
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
        .update(&Key::new("", "pods", "default", pod_name), next, Some(rv))
        .await
        .unwrap();
}

#[tokio::test]
async fn statefulset_ordered_ready_gates_creation() {
    let env = env();
    create(
        &env,
        Key::new("apps", "statefulsets", "default", "web"),
        statefulset(1),
    )
    .await;
    eventually!(15000, async { named_pod(&env, "web-0").await.is_some() });

    // web-0 not ready -> scaling to 3 must NOT create web-1.
    set_named_pod_ready(&env, "web-0", false).await;
    update_statefulset(&env, |s| s["spec"]["replicas"] = json!(3)).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        named_pod(&env, "web-1").await.is_none(),
        "OrderedReady must gate web-1 on web-0 readiness"
    );

    // Flipping web-0 ready unblocks the ordered chain: 1 then 2.
    set_named_pod_ready(&env, "web-0", true).await;
    eventually!(30000, async {
        named_pod(&env, "web-1").await.is_some() && named_pod(&env, "web-2").await.is_some()
    });
}

#[tokio::test]
async fn statefulset_rolling_update_partition() {
    let env = env();
    create(
        &env,
        Key::new("apps", "statefulsets", "default", "web"),
        statefulset(2),
    )
    .await;
    eventually!(15000, async { pods(&env).await.len() == 2 });
    let old_hash = pods(&env)
        .await
        .iter()
        .find(|p| p["metadata"]["name"] == "web-0")
        .and_then(|p| pod_revision_hash(p))
        .expect("pod carries controller-revision-hash")
        .to_string();

    // New template image + partition=1: only ordinal >= 1 rolls.
    update_statefulset(&env, |s| {
        s["spec"]["template"]["spec"]["containers"][0]["image"] = json!("nginx:1.29");
        s["spec"]["updateStrategy"] =
            json!({"type": "RollingUpdate", "rollingUpdate": {"partition": 1}});
    })
    .await;
    eventually!(30000, async {
        let Some(web1) = named_pod(&env, "web-1").await else {
            return false;
        };
        let Some(sts) = get_statefulset(&env).await else {
            return false;
        };
        pod_revision_hash(&web1).map(|h| h != old_hash) == Some(true)
            && named_pod(&env, "web-0")
                .await
                .and_then(|p| pod_revision_hash(&p).map(|h| h == old_hash))
                == Some(true)
            && status_u64(&sts, "updatedReplicas") == Some(1)
            && sts.pointer("/status/updateRevision") != sts.pointer("/status/currentRevision")
    });

    // partition=0: web-0 rolls too; currentRevision catches up.
    update_statefulset(&env, |s| {
        s["spec"]["updateStrategy"]["rollingUpdate"]["partition"] = json!(0);
    })
    .await;
    eventually!(30000, async {
        let Some(web0) = named_pod(&env, "web-0").await else {
            return false;
        };
        let Some(sts) = get_statefulset(&env).await else {
            return false;
        };
        pod_revision_hash(&web0).map(|h| h != old_hash) == Some(true)
            && sts.pointer("/status/currentRevision") == sts.pointer("/status/updateRevision")
            && status_u64(&sts, "updatedReplicas") == Some(2)
    });
    // History accumulates: both revisions are retained (no limit in v1).
    assert_eq!(revisions(&env).await.len(), 2);
}
