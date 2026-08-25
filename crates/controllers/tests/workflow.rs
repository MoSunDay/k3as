//! T7.3 Scope B acceptance (decision Q30): the Argo Workflow controller
//! drives `argoproj.io/v1alpha1` Workflows to terminal phases through the
//! real `ControllerManager` (informers + workqueues + reconciler) against
//! `EmbeddedStorage`. There is no kubelet in-process, so the tests play
//! it: step pods are CAS-moved to a `status.phase` and every wait is a
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

const NS: &str = "default";

struct Env {
    client: StorageClient,
    _stop: Stop,
}

fn env() -> Env {
    let store = Arc::new(EmbeddedStorage::new());
    let stop = Stop::new();
    let _handles = ControllerManager::spawn(store.clone(), stop.clone());
    let client = StorageClient::new(store);
    Env {
        client,
        _stop: stop,
    }
}

/// Two sequential steps groups: [gen-a] then the parallel [gen-b, gen-c].
fn steps_workflow() -> Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "two-step", "namespace": NS},
        "spec": {
            "entrypoint": "main",
            "templates": [
                {"name": "main", "steps": [
                    [{"name": "gen-a", "template": "tmpl-a"}],
                    [{"name": "gen-b", "template": "tmpl-b"},
                     {"name": "gen-c", "template": "tmpl-b"}],
                ]},
                {"name": "tmpl-a", "container": {"image": "busybox"}},
                {"name": "tmpl-b", "script": {"image": "busybox", "source": "echo hi"}},
            ],
        },
    })
}

/// Diamond DAG: A -> (B || C) -> D.
fn diamond_workflow() -> Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "diamond", "namespace": NS},
        "spec": {
            "entrypoint": "main",
            "templates": [
                {"name": "main", "dag": {"tasks": [
                    {"name": "A", "template": "work"},
                    {"name": "B", "template": "work", "dependencies": ["A"]},
                    {"name": "C", "template": "work", "depends": "A"},
                    {"name": "D", "template": "work", "depends": "B && C"},
                ]}},
                {"name": "work", "container": {"image": "busybox"}},
            ],
        },
    })
}

/// Plain container entrypoint: the root node IS the pod node.
fn container_workflow() -> Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "hello", "namespace": NS},
        "spec": {
            "entrypoint": "main",
            "templates": [
                {"name": "main", "container": {"image": "busybox"}},
            ],
        },
    })
}

fn wf_key(name: &str) -> Key {
    Key::new("argoproj.io", "workflows", NS, name)
}

async fn get_wf(env: &Env, name: &str) -> Value {
    env.client
        .get(&wf_key(name))
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("workflow {name} exists"))
}

/// Owned, non-terminating pods, mapped pod-name -> object.
async fn pods(env: &Env) -> Vec<Value> {
    env.client
        .list(&KeyPrefix::new("", "pods", Some(NS.into())))
        .await
        .unwrap()
}

async fn create_wf(env: &Env, wf: Value) {
    let name = wf["metadata"]["name"].as_str().unwrap().to_string();
    env.client.create(&wf_key(&name), wf).await.unwrap();
}

/// Play kubelet: CAS-move a pod's `status.phase`.
async fn set_pod_phase(env: &Env, pod_name: &str, phase: &str) {
    let key = Key::new("", "pods", NS, pod_name);
    let pod = env
        .client
        .get(&key)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("pod {pod_name} exists"));
    let rv = pod["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut next = pod.clone();
    next["status"] = json!({"phase": phase});
    env.client.update(&key, next, Some(rv)).await.unwrap();
}

/// Wait until a pod exists, then return its node-name annotation value.
async fn wait_pod(env: &Env, pod_name: &str) -> String {
    eventually!(10_000, async {
        let key = Key::new("", "pods", NS, pod_name);
        env.client.get(&key).await.unwrap().is_some()
    });
    let key = Key::new("", "pods", NS, pod_name);
    let pod = env.client.get(&key).await.unwrap().unwrap();
    pod["metadata"]["annotations"]["workflows.argoproj.io/node-name"]
        .as_str()
        .unwrap()
        .to_string()
}

fn node<'a>(wf: &'a Value, id: &str) -> &'a Value {
    wf.pointer(&format!("/status/nodes/{id}"))
        .unwrap_or(&Value::Null)
}

fn has_completed_condition(wf: &Value) -> bool {
    wf.pointer("/status/conditions")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .any(|c| c["type"] == "Completed" && c["status"] == "True")
        })
        .unwrap_or(false)
}

#[tokio::test]
async fn steps_workflow_runs_groups_to_succeeded() {
    let env = env();
    create_wf(&env, steps_workflow()).await;

    // Group 0 only: gating keeps later groups podless.
    let n0 = wait_pod(&env, "two-step-0-gen-a").await;
    assert_eq!(n0, "two-step[0].gen-a");
    eventually!(10_000, async {
        pods(&env).await.len() == 1
            && get_wf(&env, "two-step").await["status"]["phase"] == "Running"
    });

    // Succeed group 0 -> group 1's parallel pods appear together.
    set_pod_phase(&env, "two-step-0-gen-a", "Succeeded").await;
    let n1 = wait_pod(&env, "two-step-1-gen-b").await;
    let n2 = wait_pod(&env, "two-step-1-gen-c").await;
    assert_eq!(n1, "two-step[1].gen-b");
    assert_eq!(n2, "two-step[1].gen-c");
    set_pod_phase(&env, "two-step-1-gen-b", "Succeeded").await;
    set_pod_phase(&env, "two-step-1-gen-c", "Succeeded").await;

    eventually!(10_000, async {
        let wf = get_wf(&env, "two-step").await;
        wf["status"]["phase"] == "Succeeded" && has_completed_condition(&wf)
    });
    let wf = get_wf(&env, "two-step").await;
    let root = node(&wf, "two-step");
    assert_eq!(root["type"], "Steps");
    assert_eq!(root["phase"], "Succeeded");
    assert_eq!(root["children"], json!(["two-step[0]", "two-step[1]"]));
    let sg = node(&wf, "two-step[0]");
    assert_eq!(sg["type"], "StepGroup");
    assert_eq!(sg["phase"], "Succeeded");
    assert_eq!(sg["boundaryID"], "two-step");
    assert_eq!(sg["children"], json!(["two-step[0].gen-a"]));
    let leaf = node(&wf, "two-step[0].gen-a");
    assert_eq!(leaf["type"], "Pod");
    assert_eq!(leaf["templateName"], "tmpl-a");
    assert_eq!(leaf["phase"], "Succeeded");
    assert!(leaf["startedAt"].is_string() && leaf["finishedAt"].is_string());
}

#[tokio::test]
async fn failed_pod_fails_workflow() {
    let env = env();
    create_wf(&env, steps_workflow()).await;
    wait_pod(&env, "two-step-0-gen-a").await;

    set_pod_phase(&env, "two-step-0-gen-a", "Failed").await;
    eventually!(10_000, async {
        let wf = get_wf(&env, "two-step").await;
        wf["status"]["phase"] == "Failed" && has_completed_condition(&wf)
    });
    let wf = get_wf(&env, "two-step").await;
    assert_eq!(
        wf["status"]["message"],
        json!("step two-step[0].gen-a failed")
    );
    assert!(wf["status"]["finishedAt"].is_string());
    assert_eq!(node(&wf, "two-step[0].gen-a")["phase"], "Failed");

    // No further pods: gating stops at the failed group.
    eventually!(10_000, async { pods(&env).await.len() == 1 });
    let names: Vec<String> = pods(&env)
        .await
        .iter()
        .filter_map(|p| p["metadata"]["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(names, ["two-step-0-gen-a"]);
}

#[tokio::test]
async fn container_entrypoint_root_is_pod_node() {
    let env = env();
    create_wf(&env, container_workflow()).await;

    let n = wait_pod(&env, "hello").await; // node name == workflow name
    assert_eq!(n, "hello");
    set_pod_phase(&env, "hello", "Succeeded").await;

    eventually!(10_000, async {
        let wf = get_wf(&env, "hello").await;
        wf["status"]["phase"] == "Succeeded" && has_completed_condition(&wf)
    });
    let wf = get_wf(&env, "hello").await;
    let root = node(&wf, "hello");
    assert_eq!(root["type"], "Pod");
    assert_eq!(root["phase"], "Succeeded");
    assert_eq!(root["templateName"], "main");
    assert_eq!(root["displayName"], "hello");
    // Single-node workflow: no StepGroup/DAG scaffolding.
    assert_eq!(wf["status"]["nodes"].as_object().map(|m| m.len()), Some(1));
}

#[tokio::test]
async fn dag_workflow_layers() {
    let env = env();
    create_wf(&env, diamond_workflow()).await;

    // Layer 0: A alone.
    assert_eq!(wait_pod(&env, "diamond-a").await, "diamond.A");
    set_pod_phase(&env, "diamond-a", "Succeeded").await;

    // Layer 1: B and C in parallel (both `dependencies` and `depends` forms).
    assert_eq!(wait_pod(&env, "diamond-b").await, "diamond.B");
    assert_eq!(wait_pod(&env, "diamond-c").await, "diamond.C");
    set_pod_phase(&env, "diamond-b", "Succeeded").await;
    set_pod_phase(&env, "diamond-c", "Succeeded").await;

    // Layer 2: D only after both.
    assert_eq!(wait_pod(&env, "diamond-d").await, "diamond.D");
    eventually!(10_000, async { pods(&env).await.len() == 4 });
    set_pod_phase(&env, "diamond-d", "Succeeded").await;

    eventually!(10_000, async {
        let wf = get_wf(&env, "diamond").await;
        wf["status"]["phase"] == "Succeeded" && has_completed_condition(&wf)
    });
    let wf = get_wf(&env, "diamond").await;
    let root = node(&wf, "diamond");
    assert_eq!(root["type"], "DAG");
    assert_eq!(root["phase"], "Succeeded");
    assert_eq!(root["children"], json!(["diamond.A"]));
    for id in ["diamond.A", "diamond.B", "diamond.C", "diamond.D"] {
        assert_eq!(node(&wf, id)["type"], "Pod", "node {id}");
        assert_eq!(node(&wf, id)["phase"], "Succeeded", "node {id}");
    }
    assert_eq!(node(&wf, "diamond.D")["boundaryID"], "diamond");
}
