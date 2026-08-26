//! End-to-end kubelet tests over a REAL apiserver (TODO **T4.2**).
//!
//! Boots `apiserver::api_app` on an ephemeral port over the embedded store,
//! then drives the full kubelet loop set (`kubelet::spawn`) against a
//! `FakeCri` in-memory backend. Covers: HTTP client round-trip (GET/PUT/
//! POST/DELETE), chunked watch delivery, pod run-to-Ready with `/status`
//! writes, node registration + `kube-node-lease` heartbeat, teardown on
//! DELETE, and the kill->restart cycle (attempt+1). No containerd involved.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::SchemaRegistry;
use kubelet::{spawn, CriBackend, HttpJson, KubeletConfig};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, StorageBackend};
use support::FakeCri;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    // Four loops since Scope C: watch + sync + node/lease + local log/exec.
    assert_eq!(handles.len(), 4);

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

/// A tiny HTTP server for probe targets: returns 500 until `healthy` is
/// set, then 200 (or always 500 when `healthy` is None).
async fn spawn_probe_server(
    healthy: Option<Arc<AtomicBool>>,
) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };
            let healthy = healthy.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await; // drain the request
                let (status, body) = match healthy {
                    Some(h) if h.load(Ordering::SeqCst) => ("200 OK", "ok"),
                    _ => ("500 Internal Server Error", "not yet"),
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                    len = body.len(),
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (port, handle)
}

fn pod_with_probe(name: &str, probe_kind: &str, port: u16) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {
            "nodeName": NODE,
            "containers": [{
                "name": "c0", "image": "img:1",
                probe_kind: {
                    "httpGet": {"path": "/readyz", "port": port},
                    "initialDelaySeconds": 0,
                    "periodSeconds": 1,
                    "timeoutSeconds": 1,
                    "failureThreshold": 1,
                    "successThreshold": 1
                }
            }]
        }
    })
}

fn ready_condition(v: &Value, want: &str) -> bool {
    v["status"]["conditions"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .any(|c| c["type"] == "Ready" && c["status"] == want)
        })
        .unwrap_or(false)
}

#[tokio::test]
async fn readiness_probe_gates_ready_condition() {
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let healthy = Arc::new(AtomicBool::new(false));
    let (port, _srv) = spawn_probe_server(Some(healthy.clone())).await;
    let cri = Arc::new(FakeCri::new().with_sandbox_ip("127.0.0.1"));
    let shutdown = infra::Shutdown::new();
    let handles = spawn(
        kubelet_cfg(&url),
        cri.clone() as Arc<dyn CriBackend>,
        shutdown.clone(),
    );
    let (code, resp) = client
        .post_json(
            "/api/v1/namespaces/default/pods",
            &pod_with_probe("web", "readinessProbe", port),
        )
        .await
        .unwrap();
    assert_eq!(code, 201, "probe pod accepted: {resp}");

    // Phase Running but Ready=False while the probe 500s.
    let not_ready = wait_for(
        &client,
        "/api/v1/namespaces/default/pods/web",
        "Running with Ready=False",
        |v| v["status"]["phase"] == "Running" && ready_condition(v, "False"),
    )
    .await;
    assert_eq!(
        not_ready["status"]["containerStatuses"][0]["ready"], false,
        "probe failure keeps the container not ready"
    );

    // Flip the server healthy: Ready=True and the container turns ready.
    healthy.store(true, Ordering::SeqCst);
    wait_for(
        &client,
        "/api/v1/namespaces/default/pods/web",
        "Ready=True",
        ready_pred,
    )
    .await;
    let v = wait_for(
        &client,
        "/api/v1/namespaces/default/pods/web",
        "container ready",
        |v| v["status"]["containerStatuses"][0]["ready"] == true,
    )
    .await;
    assert!(ready_pred(&v));
    assert_eq!(v["status"]["containerStatuses"][0]["restartCount"], 0);

    shutdown.trigger();
    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
    }
}

#[tokio::test]
async fn liveness_probe_restarts_failing_container() {
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let (port, _srv) = spawn_probe_server(None).await;
    let cri = Arc::new(FakeCri::new().with_sandbox_ip("127.0.0.1"));
    let shutdown = infra::Shutdown::new();
    let handles = spawn(
        kubelet_cfg(&url),
        cri.clone() as Arc<dyn CriBackend>,
        shutdown.clone(),
    );
    let (code, resp) = client
        .post_json(
            "/api/v1/namespaces/default/pods",
            &pod_with_probe("web", "livenessProbe", port),
        )
        .await
        .unwrap();
    assert_eq!(code, 201, "probe pod accepted: {resp}");
    wait_for(
        &client,
        "/api/v1/namespaces/default/pods/web",
        "pod Running",
        ready_pred,
    )
    .await;
    // Liveness Failure (threshold 1) restarts the RUNNING container: the
    // failed container is stopped+removed and a fresh attempt starts.
    poll_until("liveness restart attempt=1", || {
        cri.peek_calls().iter().any(|c| c.starts_with("rm:"))
            && cri
                .peek_containers()
                .iter()
                .any(|c| c.attempt >= 1 && c.state == "CONTAINER_RUNNING")
    })
    .await;
    let calls = cri.peek_calls();
    assert!(
        calls.iter().filter(|c| c.starts_with("create:")).count() >= 2,
        "container recreated after liveness failure: {calls:?}"
    );
    wait_for(
        &client,
        "/api/v1/namespaces/default/pods/web",
        "restartCount >= 1",
        |v| {
            v["status"]["containerStatuses"][0]["restartCount"]
                .as_u64()
                .unwrap_or(0)
                >= 1
        },
    )
    .await;

    shutdown.trigger();
    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
    }
}

#[tokio::test]
async fn volumes_are_materialized_refreshed_and_cleaned_up() {
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let dd = data_dir();
    let mut cfg = kubelet_cfg(&url);
    cfg.data_dir = dd.clone();
    let cri = Arc::new(FakeCri::new());
    let shutdown = infra::Shutdown::new();
    let handles = spawn(cfg, cri.clone() as Arc<dyn CriBackend>, shutdown.clone());
    // (a) Create a configMap with key `greeting` via the apiserver API.
    let (code, resp) = client
        .post_json(
            "/api/v1/namespaces/default/configmaps",
            &json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "cm", "namespace": "default"},
                "data": {"greeting": "hello"},
            }),
        )
        .await
        .unwrap();
    assert_eq!(code, 201, "configMap accepted: {resp}");
    // (b) Pod mounting the configMap at /etc/cm plus an emptyDir at /data.
    let (code, resp) = client
        .post_json(
            "/api/v1/namespaces/default/pods",
            &json!({
                "apiVersion": "v1", "kind": "Pod",
                "metadata": {"name": "vpod", "namespace": "default"},
                "spec": {
                    "nodeName": NODE,
                    "volumes": [
                        {"name": "cm-vol", "configMap": {"name": "cm"}},
                        {"name": "data-vol", "emptyDir": {}},
                    ],
                    "containers": [{
                        "name": "c0", "image": "img:1",
                        "volumeMounts": [
                            {"name": "cm-vol", "mountPath": "/etc/cm", "readOnly": true},
                            {"name": "data-vol", "mountPath": "/data"},
                        ],
                    }],
                },
            }),
        )
        .await
        .unwrap();
    assert_eq!(code, 201, "volume pod accepted: {resp}");
    // Scope C upline fix: the apiserver now STAMPS metadata.uid on create,
    // so the kubelet's volume root is keyed by the real uid (not the
    // synthesized `no-uid-<ns>/<name>` fallback).
    let uid = resp["metadata"]["uid"]
        .as_str()
        .expect("pod uid stamped")
        .to_string();
    let root = dd.join("pods").join(&uid).join("volumes");
    // (c) Wait for the container to be created (the kubelet synced the pod).
    poll_until("volume pod container created", || {
        cri.peek_containers().iter().any(|c| c.name == "c0")
    })
    .await;
    let greeting = root.join("configMap").join("cm").join("greeting");
    let data_dir = root.join("emptyDir").join("data-vol");
    poll_until("configMap key materialized", || {
        std::fs::read_to_string(&greeting)
            .map(|s| s == "hello")
            .unwrap_or(false)
    })
    .await;
    assert!(
        data_dir.is_dir(),
        "emptyDir dir missing: {}",
        data_dir.display()
    );
    // (e) UPDATE the configMap -> the fingerprint refresh rewrites the file.
    let (code, mut got) = client
        .get_json("/api/v1/namespaces/default/configmaps/cm")
        .await
        .unwrap();
    assert_eq!(code, 200);
    got["data"]["greeting"] = json!("hola");
    let (code, _) = client
        .put_json("/api/v1/namespaces/default/configmaps/cm", &got)
        .await
        .unwrap();
    assert_eq!(code, 200, "configMap update accepted");
    poll_until("configMap key refreshed in place", || {
        std::fs::read_to_string(&greeting)
            .map(|s| s == "hola")
            .unwrap_or(false)
    })
    .await;
    // (f) DELETE the pod -> teardown, then the volume root is removed.
    client
        .delete_json("/api/v1/namespaces/default/pods/vpod")
        .await
        .unwrap();
    poll_until("volume root removed", || !root.exists()).await;

    shutdown.trigger();
    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
    }
}
