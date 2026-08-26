//! T7.3 Scope C acceptance (decision Q30): the CronWorkflow controller
//! fires child Workflows on schedule through the real `ControllerManager`
//! (informers + workqueues + reconciler) against `EmbeddedStorage`.
//! Schedules use `@every 2s` for speed (the 5s resync means a fire lands
//! within ~7s); every wait is a poll with timeout -- NO fixed timing
//! windows. The child Workflows run through the Scope B controller: their
//! pods never succeed in-process, so children stay active naturally; the
//! one place a child must go terminal (Forbid release) CAS-es
//! `status.phase = "Succeeded"` directly -- the Scope B terminal guard
//! (existing terminal phase -> no-op reconcile) makes it stick.

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

fn cwf_key(name: &str) -> Key {
    Key::new("argoproj.io", "cronworkflows", NS, name)
}

fn wf_key(name: &str) -> Key {
    Key::new("argoproj.io", "workflows", NS, name)
}

async fn get_cwf(env: &Env, name: &str) -> Value {
    env.client
        .get(&cwf_key(name))
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("cronworkflow {name} exists"))
}

async fn create_cwf(env: &Env, cwf: Value) {
    let name = cwf["metadata"]["name"].as_str().unwrap().to_string();
    env.client.create(&cwf_key(&name), cwf).await.unwrap();
}

/// Child Workflows controlled by the named CronWorkflow.
async fn children(env: &Env, cwf: &str) -> Vec<Value> {
    env.client
        .list(&KeyPrefix::new("argoproj.io", "workflows", Some(NS.into())))
        .await
        .unwrap()
        .into_iter()
        .filter(|wf| controller_owner_is(wf, cwf))
        .collect()
}

fn controller_owner_is(wf: &Value, cwf: &str) -> bool {
    wf.pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .map(|refs| {
            refs.iter()
                .any(|r| r["kind"] == "CronWorkflow" && r["name"] == json!(cwf))
        })
        .unwrap_or(false)
}

/// One minimal `workflowSpec`: a single container template entrypoint.
fn cron(name: &str, schedule: &str, policy: &str, suspend: bool) -> Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "CronWorkflow",
        "metadata": {"name": name, "namespace": NS},
        "spec": {
            "schedule": schedule,
            "concurrencyPolicy": policy,
            "suspend": suspend,
            "workflowSpec": {
                "entrypoint": "main",
                "templates": [
                    {"name": "main", "container": {
                        "image": "busybox",
                        "command": ["sleep", "300"],
                    }}
                ]
            }
        }
    })
}

/// CAS a child Workflow to a terminal phase (plays a finished run).
/// Retried: the Scope B controller may write its own status concurrently;
/// once this lands the terminal guard keeps it.
async fn mark_succeeded(env: &Env, wf_name: &str) -> bool {
    let Ok(Some(wf)) = env.client.get(&wf_key(wf_name)).await else {
        return false;
    };
    let rv = wf["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut next = wf.clone();
    next["status"] = json!({"phase": "Succeeded"});
    if env
        .client
        .update(&wf_key(wf_name), next, Some(rv))
        .await
        .is_err()
    {
        return false;
    }
    let fresh = env.client.get(&wf_key(wf_name)).await.unwrap().unwrap();
    fresh["status"]["phase"] == "Succeeded"
}

#[tokio::test]
async fn cron_fires_child_workflow() {
    let env = env();
    create_cwf(&env, cron("tick", "@every 2s", "Allow", false)).await;

    eventually!(15_000, async { !children(&env, "tick").await.is_empty() });
    let kids = children(&env, "tick").await;
    let child = &kids[0];
    let child_name = child["metadata"]["name"].as_str().unwrap().to_string();
    assert_eq!(child["kind"], "Workflow");
    // Parent label (upstream convention) + verbatim workflowSpec.
    assert_eq!(
        child["metadata"]["labels"]["workflows.argoproj.io/cron-workflow"],
        "tick"
    );
    assert_eq!(child["spec"]["entrypoint"], "main");
    assert_eq!(child["spec"]["templates"].as_array().unwrap().len(), 1);
    // The Scope B controller drives the child (planned pods -> Running).
    eventually!(10_000, async {
        let wf = env.client.get(&wf_key(&child_name)).await.unwrap().unwrap();
        wf["status"]["phase"] == "Running"
    });
    // CronWorkflow status: fired + active child reference.
    eventually!(10_000, async {
        let cwf = get_cwf(&env, "tick").await;
        !cwf["status"]["active"].as_array().unwrap().is_empty()
            && cwf["status"]["lastScheduledTime"].as_str().is_some()
    });
    let cwf = get_cwf(&env, "tick").await;
    let active = cwf["status"]["active"].as_array().unwrap();
    assert_eq!(active[0]["kind"], "Workflow");
    assert_eq!(active[0]["name"], json!(child_name));
    assert!(active[0]["apiVersion"]
        .as_str()
        .unwrap()
        .contains("argoproj.io"));
}

#[tokio::test]
async fn forbid_blocks_while_active() {
    let env = env();
    create_cwf(&env, cron("forbid", "@every 2s", "Forbid", false)).await;

    eventually!(15_000, async { children(&env, "forbid").await.len() == 1 });
    let first_name = children(&env, "forbid").await[0]["metadata"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let last = get_cwf(&env, "forbid").await["status"]["lastScheduledTime"].clone();

    // >= 2 more schedule windows (2s each, plus resync slack) pass while
    // the child stays active: Forbid skips every due tick.
    tokio::time::sleep(Duration::from_secs(8)).await;
    let kids = children(&env, "forbid").await;
    assert_eq!(
        kids.len(),
        1,
        "Forbid must not fire while a child is active"
    );
    assert_eq!(kids[0]["metadata"]["name"], json!(first_name));
    // Upstream parity: skipped ticks do NOT advance lastScheduledTime.
    assert_eq!(
        get_cwf(&env, "forbid").await["status"]["lastScheduledTime"],
        last
    );

    // Child goes terminal (terminal guard makes the CAS stick): Forbid
    // releases and a SECOND child fires.
    eventually!(15_000, async {
        mark_succeeded(&env, &first_name).await && children(&env, "forbid").await.len() == 2
    });
    let cwf = get_cwf(&env, "forbid").await;
    assert_eq!(
        cwf["status"]["active"].as_array().unwrap().len(),
        1,
        "the terminal child drops out of active"
    );
}

#[tokio::test]
async fn replace_deletes_active() {
    let env = env();
    create_cwf(&env, cron("swap", "@every 2s", "Replace", false)).await;

    eventually!(15_000, async { !children(&env, "swap").await.is_empty() });
    let first_name = children(&env, "swap").await[0]["metadata"]["name"]
        .as_str()
        .unwrap()
        .to_string();

    // The next due tick deletes the active child and fires exactly one
    // new one (never two coexisting children).
    eventually!(20_000, async {
        let kids = children(&env, "swap").await;
        kids.len() == 1 && kids[0]["metadata"]["name"].as_str() != Some(first_name.as_str())
    });
    // The first child is really gone (Replace deleted it).
    assert!(env
        .client
        .get(&wf_key(&first_name))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn suspend_never_fires() {
    let env = env();
    create_cwf(&env, cron("paused", "@every 2s", "Allow", true)).await;

    // The reconciler runs (status maintained, active empty) but no child
    // ever appears -- suspend stops all firing.
    eventually!(15_000, async {
        get_cwf(&env, "paused").await["status"]["active"] == json!([])
    });
    assert!(children(&env, "paused").await.is_empty());
    assert!(
        get_cwf(&env, "paused").await["status"]["lastScheduledTime"].is_null(),
        "suspend must not record a schedule time"
    );
}
