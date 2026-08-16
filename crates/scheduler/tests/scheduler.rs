//! T3.2 acceptance: the real `SchedulerManager` (leader election + pod/node
//! informers + pending-pod queue + filter/score cycle + bind/unschedulable
//! writers) against `EmbeddedStorage`, and the HTTP extender seam exercised
//! end-to-end through an in-process axum stub (the G23 shape). Every wait
//! is a poll with timeout -- NO fixed timing windows; the unschedulable
//! case additionally asserts quiesce (no revision churn after settle).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::post;
use axum::Json;
use axum::Router;
use controllers::{Client, Stop, StorageClient};
use scheduler::{ExtenderConfig, SchedulerConfig, SchedulerManager};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, StorageBackend};

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
    #[allow(dead_code)]
    _handles: Vec<tokio::task::JoinHandle<()>>,
}

fn env(cfg: SchedulerConfig) -> Env {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let stop = Stop::new();
    let handles = SchedulerManager::spawn(store.clone(), cfg, stop.clone());
    Env {
        client: StorageClient::new(store),
        _stop: stop,
        _handles: handles,
    }
}

fn node(name: &str, labels: Value) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {"name": name, "labels": labels},
    })
}

fn pod(name: &str, selector: Value) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {
            "nodeSelector": selector,
            "containers": [{"name": "c", "image": "pause"}],
        },
    })
}

async fn create_pod(env: &Env, p: Value) {
    let name = p["metadata"]["name"].as_str().unwrap().to_string();
    env.client
        .create(&Key::new("", "pods", "default", &name), p)
        .await
        .unwrap();
}

async fn create_node(env: &Env, n: Value) {
    let name = n["metadata"]["name"].as_str().unwrap().to_string();
    env.client
        .create(&Key::new("", "nodes", "", &name), n)
        .await
        .unwrap();
}

async fn get_pod(env: &Env, name: &str) -> Value {
    env.client
        .get(&Key::new("", "pods", "default", name))
        .await
        .unwrap()
        .expect("pod exists")
}

fn scheduled_true(pod: &Value) -> bool {
    pod.pointer("/status/conditions")
        .and_then(|v| v.as_array())
        .map(|cs| {
            cs.iter().any(|c| {
                c.get("type").and_then(|v| v.as_str()) == Some("PodScheduled")
                    && c.get("status").and_then(|v| v.as_str()) == Some("True")
            })
        })
        .unwrap_or(false)
}

fn unschedulable(pod: &Value) -> bool {
    pod.pointer("/status/conditions")
        .and_then(|v| v.as_array())
        .map(|cs| {
            cs.iter().any(|c| {
                c.get("type").and_then(|v| v.as_str()) == Some("PodScheduled")
                    && c.get("status").and_then(|v| v.as_str()) == Some("False")
                    && c.get("reason").and_then(|v| v.as_str()) == Some("Unschedulable")
            })
        })
        .unwrap_or(false)
}

#[tokio::test]
async fn binds_pending_pod_to_labeled_node() {
    let env = env(SchedulerConfig::new());
    create_node(&env, node("a", json!({"disk": "ssd"}))).await;
    create_node(&env, node("b", json!({}))).await;
    create_pod(&env, pod("web", json!({"disk": "ssd"}))).await;

    eventually!(15_000, async {
        let p = get_pod(&env, "web").await;
        p.pointer("/spec/nodeName").and_then(|v| v.as_str()) == Some("a")
    });
    eventually!(15_000, async {
        scheduled_true(&get_pod(&env, "web").await)
    });
}

#[tokio::test]
async fn node_appearing_late_retries_unschedulable_pod() {
    let env = env(SchedulerConfig::new());
    create_node(&env, node("b", json!({}))).await;
    create_pod(&env, pod("web", json!({"disk": "ssd"}))).await;

    // Nothing matches yet: Unschedulable, and it stays calm (no churn).
    eventually!(15_000, async { unschedulable(&get_pod(&env, "web").await) });

    // The matching node arrives: the node event fans out to pending pods.
    create_node(&env, node("a", json!({"disk": "ssd"}))).await;
    eventually!(15_000, async {
        get_pod(&env, "web")
            .await
            .pointer("/spec/nodeName")
            .and_then(|v| v.as_str())
            == Some("a")
    });
}

#[tokio::test]
async fn unschedulable_pod_quiesces_instead_of_hot_looping() {
    let env = env(SchedulerConfig::new());
    create_node(&env, node("a", json!({}))).await;
    create_pod(&env, pod("web", json!({"disk": "does-not-exist"}))).await;

    eventually!(15_000, async { unschedulable(&get_pod(&env, "web").await) });

    // Settled: no further revision churn for a generous window (the pod is
    // only re-enqueued by pod/node events or the 30s backstop, never by
    // the worker loop itself).
    let rv = get_pod(&env, "web").await["metadata"]["resourceVersion"].clone();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = get_pod(&env, "web").await["metadata"]["resourceVersion"].clone();
    assert_eq!(rv, after, "unschedulable pod must not hot-loop writes");
}

// ---- extender seam (G23 shape, in-process stub) ----

async fn spawn_stub(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

fn extender_cfg(url: &str) -> ExtenderConfig {
    serde_json::from_value(json!({"urlPrefix": url})).unwrap()
}

async fn filter_reject_all(Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({"NodeNames": []}))
}

#[tokio::test]
async fn extender_filter_rejecting_all_nodes_leaves_pod_unschedulable() {
    let url = spawn_stub(Router::new().route("/filter", post(filter_reject_all))).await;
    let mut cfg = SchedulerConfig::new();
    cfg.extenders = vec![extender_cfg(&url)];
    let env = env(cfg);
    create_node(&env, node("a", json!({}))).await;
    create_pod(&env, pod("web", json!({}))).await;

    eventually!(15_000, async { unschedulable(&get_pod(&env, "web").await) });
}

async fn prioritize_second_node(Json(body): Json<Value>) -> Json<Value> {
    let names: Vec<String> = body
        .pointer("/nodes/Items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|n| {
                    n.pointer("/metadata/name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    // Local scores tie on identical logical nodes; the extender breaks it
    // in favour of the last node in the list.
    let winner = names.last().cloned().unwrap_or_default();
    let scores: Vec<Value> = names
        .iter()
        .map(|n| json!({"host": n, "score": if *n == winner { 100 } else { 0 }}))
        .collect();
    Json(Value::Array(scores))
}

async fn filter_accept_all(Json(body): Json<Value>) -> Json<Value> {
    // Echo back every node name we were asked about.
    let names: Vec<String> = body
        .pointer("/nodes/Items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|n| {
                    n.pointer("/metadata/name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    Json(json!({"NodeNames": names}))
}

#[tokio::test]
async fn extender_prioritize_steers_placement_among_equal_nodes() {
    let url = spawn_stub(
        Router::new()
            .route("/filter", post(filter_accept_all))
            .route("/prioritize", post(prioritize_second_node)),
    )
    .await;
    let mut cfg = SchedulerConfig::new();
    cfg.extenders = vec![extender_cfg(&url)];
    let env = env(cfg);
    create_node(&env, node("n1", json!({}))).await;
    create_node(&env, node("n2", json!({}))).await;
    create_pod(&env, pod("web", json!({}))).await;

    eventually!(15_000, async {
        get_pod(&env, "web")
            .await
            .pointer("/spec/nodeName")
            .and_then(|v| v.as_str())
            .map(|n| n == "n2")
            .unwrap_or(false)
    });
}
