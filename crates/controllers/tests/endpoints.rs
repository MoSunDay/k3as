//! Sprint 18 / S2 acceptance: Endpoints targetPort resolution + real podIP
//! addresses, through the real `ControllerManager` against `EmbeddedStorage`
//! (split out of controllers.rs for the file-size cap).

use std::sync::Arc;
use std::time::{Duration, Instant};

use controllers::{Client, ControllerManager, Stop, StorageClient};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key};

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
        client: StorageClient::new(store),
        _stop: stop,
    }
}

async fn create(env: &Env, key: Key, obj: Value) {
    env.client.create(&key, obj).await.unwrap();
}

/// Address count of the first Endpoints subset (0 when absent/empty).
fn subset_addresses(ep: &Value) -> usize {
    ep["subsets"]
        .as_array()
        .and_then(|s| s.first())
        .and_then(|s| s["addresses"].as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

#[tokio::test]
async fn endpoints_prefer_real_pod_ip() {
    let env = env();
    // Bare pod (no Deployment) with a kubelet-reported podIP (Sprint 18 / S1).
    create(
        &env,
        Key::new("", "pods", "default", "direct-0"),
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "direct-0", "namespace": "default",
                         "labels": {"app": "direct"}},
            "spec": {"containers": [{"name": "c", "image": "nginx:1.0"}]},
            "status": {
                "phase": "Running",
                "podIP": "10.42.0.7",
                "podIPs": [{"ip": "10.42.0.7"}],
                "conditions": [{"type": "Ready", "status": "True"}],
            },
        }),
    )
    .await;
    create(
        &env,
        Key::new("", "services", "default", "direct"),
        json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": "direct", "namespace": "default"},
            "spec": {
                "selector": {"app": "direct"},
                "ports": [{"port": 80, "targetPort": 8080}],
            },
        }),
    )
    .await;

    let ep_key = Key::new("", "endpoints", "default", "direct");
    eventually!(5000, async {
        env.client
            .get(&ep_key)
            .await
            .unwrap()
            .is_some_and(|ep| subset_addresses(&ep) == 1)
    });
    let ep = env.client.get(&ep_key).await.unwrap().unwrap();
    // Real podIP wins over the deterministic placeholder.
    assert_eq!(ep["subsets"][0]["addresses"][0]["ip"], "10.42.0.7");
    assert_eq!(ep["subsets"][0]["ports"][0]["port"], 8080);
}

#[tokio::test]
async fn endpoints_named_target_port_resolved_from_pod() {
    let env = env();
    create(
        &env,
        Key::new("", "pods", "default", "named-0"),
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "named-0", "namespace": "default",
                         "labels": {"app": "named"}},
            "spec": {"containers": [{"name": "c", "image": "x", "ports": [
                {"name": "web", "containerPort": 9000},
            ]}]},
            "status": {
                "phase": "Running",
                "podIP": "10.42.0.9",
                "podIPs": [{"ip": "10.42.0.9"}],
                "conditions": [{"type": "Ready", "status": "True"}],
            },
        }),
    )
    .await;
    create(
        &env,
        Key::new("", "services", "default", "named"),
        json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": "named", "namespace": "default"},
            "spec": {
                "selector": {"app": "named"},
                "ports": [{"name": "http", "port": 80, "targetPort": "web"}],
            },
        }),
    )
    .await;

    let ep_key = Key::new("", "endpoints", "default", "named");
    eventually!(5000, async {
        env.client
            .get(&ep_key)
            .await
            .unwrap()
            .is_some_and(|ep| subset_addresses(&ep) == 1)
    });
    let ep = env.client.get(&ep_key).await.unwrap().unwrap();
    // Named targetPort resolved from the pod's containerPorts; Endpoints
    // carry the container port under the Service port name.
    assert_eq!(ep["subsets"][0]["ports"][0]["port"], 9000);
    assert_eq!(ep["subsets"][0]["ports"][0]["name"], "http");
}
