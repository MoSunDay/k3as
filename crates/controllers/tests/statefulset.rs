//! StatefulSet controller acceptance (TODO **T3.1b** slice S3, split out of
//! `controllers.rs` to keep both files under the 800-line cap): in-process
//! e2e through the real `ControllerManager` against `EmbeddedStorage`.
//!
//! Readiness duality: status COUNTERS stay ready-by-default (T3.1a) while
//! the OrderedReady ordinal GATE is strict (T3.1b, golden G61). No kubelet
//! here, so the tests play kubelet (T4.2): each ordinal's `Ready` condition
//! is pinned as soon as it appears (golden G19 does the same over REST).

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
    Env {
        client: StorageClient::new(store.clone()),
        _stop: stop,
    }
}

async fn create(env: &Env, key: Key, obj: Value) {
    env.client.create(&key, obj).await.unwrap();
}

async fn pods(env: &Env) -> Vec<Value> {
    env.client
        .list(&KeyPrefix::new("", "pods", Some("default".into())))
        .await
        .unwrap()
}

fn status_u64(dep: &Value, field: &str) -> Option<u64> {
    dep.pointer(&format!("/status/{field}"))
        .and_then(Value::as_u64)
}

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
    // No kubelet here: the test reports readiness per ordinal (T4.2 parity)
    // so the OrderedReady gate opens the chain 0 -> 1 -> 2.
    ordered_ready_pods(&env, 3).await;
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
    // Kubelet-parity walk (T4.2): gate each ordinal open with Ready=True.
    ordered_ready_pods(&env, 3).await;

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

/// Kubelet-parity ordinal walk for OrderedReady tests (T3.1b gate + T4.2
/// status writer): wait for `web-<i>` to appear, then CAS-pin Ready=True so
/// the strict gate opens ordinal i+1. Tests here run WITHOUT a kubelet, so
/// the test itself plays the kubelet -- exactly what golden G19 does over
/// REST (`PUT pods/status`) and golden G61 gets for free from the real agent.
async fn ordered_ready_pods(env: &Env, n: usize) {
    for i in 0..n {
        let pod_name = format!("web-{i}");
        eventually!(15000, async { named_pod(env, &pod_name).await.is_some() });
        set_named_pod_ready(env, &pod_name, true).await;
    }
    eventually!(15000, async { pods(env).await.len() == n });
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

    // Flipping web-0 ready unblocks the ordered chain ONE ordinal at a
    // time: web-1 appears, but web-2 must still wait for web-1's own
    // Ready condition (the G61 semantics).
    set_named_pod_ready(&env, "web-0", true).await;
    eventually!(30000, async { named_pod(&env, "web-1").await.is_some() });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        named_pod(&env, "web-2").await.is_none(),
        "condition-less web-1 must hold back web-2 (one ordinal per reconcile)"
    );
    set_named_pod_ready(&env, "web-1", true).await;
    eventually!(30000, async { named_pod(&env, "web-2").await.is_some() });
}

/// Regression for golden G61 (plan item D2): a FRESHLY created pod (no
/// status yet) must count as NOT ready in the OrderedReady gate. The
/// removed bug flipped `prev_ready = true` right after a fresh create, so
/// replicas=3 materialized ordinals 0+1 in ONE reconcile before any
/// kubelet had reported anything.
#[tokio::test]
async fn statefulset_gate_blocks_next_ordinal_on_conditionless_pod() {
    let env = env();
    create(
        &env,
        Key::new("apps", "statefulsets", "default", "web"),
        statefulset(3),
    )
    .await;
    // web-0 appears; over an observation window web-1 must NEVER show up
    // while no Ready condition exists (negative assertion, so a bounded
    // window is the strongest statement available).
    eventually!(15000, async { named_pod(&env, "web-0").await.is_some() });
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            named_pod(&env, "web-1").await.is_none(),
            "condition-less web-0 must hold back web-1 (OrderedReady, golden G61)"
        );
    }
    // Kubelet parity: reporting Ready=True on web-0 opens ordinal 1; web-2
    // stays gated until web-1 is reported ready in turn.
    set_named_pod_ready(&env, "web-0", true).await;
    eventually!(30000, async { named_pod(&env, "web-1").await.is_some() });
    assert!(
        named_pod(&env, "web-2").await.is_none(),
        "web-2 must wait for web-1's Ready condition"
    );
    set_named_pod_ready(&env, "web-1", true).await;
    eventually!(30000, async { named_pod(&env, "web-2").await.is_some() });
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
    // Kubelet-parity walk (T4.2): both ordinals exist AND are Ready, so the
    // rolling-update assertions below see a converged baseline.
    ordered_ready_pods(&env, 2).await;
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
