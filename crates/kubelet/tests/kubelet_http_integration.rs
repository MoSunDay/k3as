//! End-to-end kubelet tests over a REAL apiserver (TODO **T4.2**).
//!
//! Boots `apiserver::api_app` on an ephemeral port over the embedded store,
//! then drives the full kubelet loop set (`kubelet::spawn`) against a
//! `FakeCri` in-memory backend. Covers: HTTP client round-trip (GET/PUT/
//! POST/DELETE), chunked watch delivery, pod run-to-Ready with `/status`
//! writes, node registration + `kube-node-lease` heartbeat, teardown on
//! DELETE, and the kill->restart cycle (attempt+1). No containerd involved.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::SchemaRegistry;
use kubelet::{spawn, CriBackend, HttpJson, KubeletConfig};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, StorageBackend};
use support::FakeCri;
use tokio::net::TcpListener;

const NODE: &str = "test-node";

// ---- server + helpers ----------------------------------------------------

fn served() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    reg.register_native::<k8s_openapi::api::coordination::v1::Lease>();
    reg
}

/// Boot the real apiserver app on an ephemeral port; returns the base URL.
async fn spawn_server() -> String {
    let store: Arc<dyn StorageBackend> = Arc::new(EmbeddedStorage::new());
    let app = apiserver::api_app(Arc::new(served()), store, "127.0.0.1:6443".to_string());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    format!("http://{addr}")
}

static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

fn data_dir() -> std::path::PathBuf {
    let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("init-pro-kubelet-it-{}-{n}", std::process::id()))
}

fn kubelet_cfg(url: &str) -> KubeletConfig {
    let mut cfg = KubeletConfig::new(url.to_string(), NODE, data_dir());
    cfg.sync_period = Duration::from_millis(100);
    cfg.heartbeat_period = Duration::from_millis(200);
    cfg
}

fn pod_object(name: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {
            "nodeName": NODE,
            "containers": [{"name": "c0", "image": "img:1"}]
        }
    })
}

/// Poll GET `path` until `pred` holds; fails the test after 20s.
async fn wait_for(
    client: &HttpJson,
    path: &str,
    what: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok((200, v)) = client.get_json(path).await {
            if pred(&v) {
                return v;
            }
        }
        assert!(Instant::now() < deadline, "timeout waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll a cheap sync predicate until true; fails the test after 20s.
async fn poll_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timeout waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn ready_pred(v: &Value) -> bool {
    v["status"]["phase"] == "Running"
        && v["status"]["conditions"]
            .as_array()
            .map(|cs| {
                cs.iter()
                    .any(|c| c["type"] == "Ready" && c["status"] == "True")
            })
            .unwrap_or(false)
}

// ---- tests ----------------------------------------------------------------

#[tokio::test]
async fn http_get_put_post_delete_round_trip() {
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let (code, created) = client
        .post_json("/api/v1/namespaces/default/pods", &pod_object("web"))
        .await
        .unwrap();
    assert_eq!(code, 201, "POST pod: {created}");
    let (code, got) = client
        .get_json("/api/v1/namespaces/default/pods/web")
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(got["spec"]["nodeName"], NODE);
    let (code, st) = client
        .put_json(
            "/api/v1/namespaces/default/pods/web/status",
            &json!({"status": {"phase": "Running"}}),
        )
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(st["status"]["phase"], "Running");
    let (code, _) = client
        .delete_json("/api/v1/namespaces/default/pods/web")
        .await
        .unwrap();
    assert_eq!(code, 200);
    let (code, _) = client
        .get_json("/api/v1/namespaces/default/pods/web")
        .await
        .unwrap();
    assert_eq!(code, 404);
}

#[tokio::test]
async fn watch_delivers_events() {
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let mut conn = client
        .watch("/api/v1/pods?watch=1&resourceVersion=0")
        .await
        .unwrap();
    client
        .post_json("/api/v1/namespaces/default/pods", &pod_object("w1"))
        .await
        .unwrap();
    let line = tokio::time::timeout(Duration::from_secs(10), conn.next_line())
        .await
        .expect("watch line timed out")
        .expect("watch stream ended early");
    let ev: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(ev["type"], "ADDED");
    assert_eq!(ev["object"]["metadata"]["name"], "w1");
}

#[tokio::test]
async fn kubelet_runs_pod_via_fake_cri_and_tears_down_on_delete() {
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let cri = Arc::new(FakeCri::new());
    let shutdown = infra::Shutdown::new();
    let handles = spawn(
        kubelet_cfg(&url),
        cri.clone() as Arc<dyn CriBackend>,
        shutdown.clone(),
    );
    assert_eq!(handles.len(), 3);

    // Node registration + lease heartbeat happen before any pod exists.
    wait_for(
        &client,
        &format!("/api/v1/nodes/{NODE}"),
        "node object",
        |v| v["metadata"]["name"] == NODE,
    )
    .await;
    let lease_path =
        format!("/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{NODE}");
    let lease = wait_for(&client, &lease_path, "node lease", |v| {
        v["spec"]["holderIdentity"] == NODE
    })
    .await;
    assert!(lease["spec"]["renewTime"]
        .as_str()
        .unwrap_or("")
        .contains('T'));
    wait_for(
        &client,
        &format!("/api/v1/nodes/{NODE}"),
        "node Ready",
        |v| {
            v["status"]["conditions"]
                .as_array()
                .map(|cs| {
                    cs.iter()
                        .any(|c| c["type"] == "Ready" && c["status"] == "True")
                })
                .unwrap_or(false)
        },
    )
    .await;

    // Schedule a pod; the kubelet must run it to Running+Ready.
    client
        .post_json("/api/v1/namespaces/default/pods", &pod_object("web"))
        .await
        .unwrap();
    wait_for(
        &client,
        "/api/v1/namespaces/default/pods/web",
        "pod Running+Ready",
        ready_pred,
    )
    .await;

    // Sprint 18 / S1: the READY sandbox's CNI IP surfaces in the kubelet's
    // /status writes (podIP + podIPs + hostIP all present).
    let with_ip = wait_for(
        &client,
        "/api/v1/namespaces/default/pods/web",
        "pod podIP",
        |v| {
            v["status"]["podIP"].is_string()
                && v["status"]["podIPs"][0]["ip"] == v["status"]["podIP"]
                && v["status"]["hostIP"] == "127.0.0.1"
        },
    )
    .await;
    assert_eq!(
        with_ip["status"]["podIP"].as_str(),
        cri.peek_sandboxes()[0].ip.as_deref(),
        "reported podIP is the sandbox CNI IP"
    );

    let calls = cri.peek_calls();
    for prefix in ["pull:", "runp:", "create:", "start:"] {
        assert!(
            calls.iter().any(|c| c.starts_with(prefix)),
            "missing {prefix} in {calls:?}"
        );
    }
    let sbs = cri.peek_sandboxes();
    assert_eq!(sbs.len(), 1);
    assert!(!sbs[0].uid.is_empty());
    let cts = cri.peek_containers();
    assert_eq!(cts.len(), 1);
    assert_eq!(cts[0].attempt, 0);
    assert_eq!(cts[0].state, "CONTAINER_RUNNING");

    // DELETE -> full teardown (containers removed, sandbox gone).
    client
        .delete_json("/api/v1/namespaces/default/pods/web")
        .await
        .unwrap();
    poll_until("sandbox teardown", || cri.peek_sandboxes().is_empty()).await;
    assert!(cri.peek_containers().is_empty());
    let calls = cri.peek_calls();
    assert!(
        calls.iter().any(|c| c.starts_with("rmp:")),
        "no sandbox remove in {calls:?}"
    );

    shutdown.trigger();
    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
    }
}

#[tokio::test]
async fn kill_restart_cycle_recreates_with_attempt_plus_one() {
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let cri = Arc::new(FakeCri::new());
    let shutdown = infra::Shutdown::new();
    let handles = spawn(
        kubelet_cfg(&url),
        cri.clone() as Arc<dyn CriBackend>,
        shutdown.clone(),
    );
    client
        .post_json("/api/v1/namespaces/default/pods", &pod_object("web"))
        .await
        .unwrap();
    wait_for(
        &client,
        "/api/v1/namespaces/default/pods/web",
        "pod Running",
        ready_pred,
    )
    .await;

    // Kill the container; the kubelet must recreate it with attempt = 1.
    let victim = cri.peek_containers()[0].id.clone();
    cri.flip_container(&victim, "CONTAINER_EXITED");
    poll_until("recreated container attempt=1 running", || {
        cri.peek_containers()
            .iter()
            .any(|c| c.attempt == 1 && c.state == "CONTAINER_RUNNING")
    })
    .await;
    // The exited original container was removed; exactly one remains.
    assert_eq!(cri.peek_containers().len(), 1);
    let calls = cri.peek_calls();
    assert!(
        calls.iter().any(|c| c == &format!("rm:{victim}")),
        "no rm of {victim} in {calls:?}"
    );

    shutdown.trigger();
    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
    }
}
