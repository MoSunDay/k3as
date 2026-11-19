//! T8.5 acceptance (decision **Q58**): pipeline-owned canary release through
//! the real `ControllerManager`, a REAL embedded registry over loopback and a
//! fake `docker` CLI shim (the T8.3 harness of `tests/pipeline.rs`, Q56).
//! Flow: `spec.release` -> `Build` -> canary Deployment `web-canary` + canary
//! Service `web-stable-canary` cloned from the stable pair -> stepped
//! `TrafficSplit` weights (50 -> soak -> 100) -> promote, then abort / GC. No
//! kubelet: assertions read stored objects, never pods or Endpoints. Phase
//! strings mirror `api::pipeline::PHASE_*` (`Rolling`/`Promoted`/`Aborted`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use controllers::{Client, ControllerManager, ManagerConfig, PipelineConfig, Stop, StorageClient};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, KeyPrefix};
use tokio::task::JoinHandle;

/// Poll `cond` (an async block) until true, panicking with `msg` after `ms`.
macro_rules! eventually {
    ($ms:expr, $msg:expr, $cond:expr) => {{
        let deadline = Instant::now() + Duration::from_millis($ms);
        loop {
            if $cond.await {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {}", $msg);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }};
}

/// Per-binary fixture-dir sequence: pid + millis alone collide within a ms.
static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_tmp_dir() -> PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let stamp = registry::now_millis();
    let dir = std::env::temp_dir().join(format!("init-pro-canary-{pid}-{stamp}-{seq}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Live harness: manager handles + `stop` stay alive for the whole test
/// (mirrors `tests/controllers.rs`); the fixture dir is reaped by `Drop`.
struct Env {
    client: StorageClient,
    /// Registry authority (`127.0.0.1:<port>`) the controller pushes to.
    addr: String,
    tmp: PathBuf,
    _stop: Stop,
    _handles: Vec<JoinHandle<()>>,
}

impl Drop for Env {
    fn drop(&mut self) {
        // Best effort: `/tmp` here is ZFS, which can transiently answer
        // `rmdir` with `ENOTEMPTY`; retry a few times, never fail the test.
        for _ in 0..10 {
            if std::fs::remove_dir_all(&self.tmp).is_ok() || !self.tmp.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Boot the REAL registry (V2 router) on an ephemeral loopback port.
async fn boot_registry(tmp: &Path) -> String {
    let root = tmp.join("registry");
    registry::store::prepare(&root).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = registry::v2_router(root, registry::RegistryConfig::default());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("registry server task failed: {e}");
        }
    });
    addr
}

/// A plain (uncompressed) `docker save` archive, as the shim produces.
fn write_fixture(tmp: &Path) -> PathBuf {
    fn append(builder: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, name, data).unwrap();
    }
    let manifest = br#"[{"Config":"cfg.json","RepoTags":["team/web:v1"],"Layers":["layer.tar"]}]"#;
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let layer = b"layer bytes for the canary e2e\n";
    let mut builder = tar::Builder::new(Vec::new());
    append(&mut builder, "manifest.json", manifest);
    append(&mut builder, "cfg.json", config);
    append(&mut builder, "layer.tar", layer);
    let path = tmp.join("fixture.tar");
    std::fs::write(&path, builder.into_inner().unwrap()).unwrap();
    path
}

/// Fake `docker`: `build` succeeds, `save -o <tar> <ref>` copies the fixture.
fn write_docker_shim(tmp: &Path, fixture: &Path) -> String {
    let script = tmp.join("docker");
    let body = format!(
        "#!/bin/sh\n\
         case \"$1\" in\n\
         build) echo \"fake docker build ok\"; exit 0;;\n\
         save) cp \"{}\" \"$3\"; echo \"fake docker save ok\"; exit 0;;\n\
         *) echo \"unexpected fake docker args: $*\" >&2; exit 2;;\n\
         esac\n",
        fixture.display()
    );
    std::fs::write(&script, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script.display().to_string()
}

/// Boot the manager with the pipeline plane ON, registry up, shim ready.
async fn boot_env() -> Env {
    let tmp = unique_tmp_dir();
    let addr = boot_registry(&tmp).await;
    let fixture = write_fixture(&tmp);
    let docker_bin = write_docker_shim(&tmp, &fixture);
    let store = Arc::new(EmbeddedStorage::new());
    let stop = Stop::new();
    let cfg = ManagerConfig {
        pipeline: Some(PipelineConfig {
            registry_addr: addr.clone(),
            scratch_dir: tmp.join("scratch"),
            docker_bin,
            default_timeout_seconds: 30,
        }),
        ..Default::default()
    };
    let handles = ControllerManager::spawn_with(store.clone(), stop.clone(), cfg);
    let client = StorageClient::new(store);
    Env {
        client,
        addr,
        tmp,
        _stop: stop,
        _handles: handles,
    }
}

/// Storage key of a `default`-namespace object (group, resource, name).
fn key(group: &str, resource: &str, name: &str) -> Key {
    Key::new(group, resource, "default", name)
}

async fn stored(client: &StorageClient, key: &Key) -> Value {
    client.get(key).await.unwrap().unwrap_or(Value::Null)
}

async fn builds(client: &StorageClient) -> Vec<Value> {
    let prefix = KeyPrefix::new("init-pro.io", "builds", Some("default".to_string()));
    client.list(&prefix).await.unwrap()
}

/// Stable Deployment `web` (image `team/web:v0`): the canary clone source.
fn stable_deployment() -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "replicas": 1, "selector": {"matchLabels": {"app": "web"}},
            "template": {
                "metadata": {"labels": {"app": "web"}},
                "spec": {"containers": [{"name": "app", "image": "team/web:v0"}]},
            },
        },
    })
}

/// Stable Service `web-stable`: NodePort on purpose, so the clone must strip
/// `nodePort` and fall back to `ClusterIP`.
fn stable_service() -> Value {
    json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": {"name": "web-stable", "namespace": "default"},
        "spec": {
            "type": "NodePort", "selector": {"app": "web"},
            "ports": [{"name": "http", "port": 80, "targetPort": 8080, "protocol": "TCP"}],
        },
    })
}

/// The promoted repository carries the registry host (`<addr>/team/web:v1`).
fn pipeline(addr: &str, context: &Path, release: Value) -> Value {
    json!({
        "apiVersion": "init-pro.io/v1", "kind": "ServicePipeline",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "image": {"repository": format!("{addr}/team/web"), "tag": "v1"},
            "source": {"context": context.display().to_string()},
            "build": {"timeoutSeconds": 30},
            "target": {"deployment": "web", "container": "app"},
            "release": release,
        },
    })
}

/// Seed the stable pair + the entry `TrafficSplit` + the pipeline.
async fn seed(env: &Env, release: Value) {
    let src = env.tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let split = json!({
        "apiVersion": "init-pro.io/v1", "kind": "TrafficSplit",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {"service": "web-entry", "backends": [{"service": "web-stable", "weight": 100}]},
    });
    let objects = [
        (key("apps", "deployments", "web"), stable_deployment()),
        (key("", "services", "web-stable"), stable_service()),
        (key("init-pro.io", "trafficsplits", "web"), split),
        (
            key("init-pro.io", "servicepipelines", "web"),
            pipeline(&env.addr, &src, release),
        ),
    ];
    for (key, object) in objects {
        env.client.create(&key, object).await.unwrap();
    }
}

fn image_of(dep: &Value) -> Option<&str> {
    dep.pointer("/spec/template/spec/containers/0/image")
        .and_then(Value::as_str)
}

/// Q58 D2 keys at `path`: ownership (`pipeline` + `canary`) and, where
/// `revision` is set, the non-empty per-tag revision label.
fn assert_labels(object: &Value, path: &str, revision: bool) {
    let l = |k: &str| {
        object
            .pointer(path)
            .and_then(|p| p.get(k))
            .and_then(Value::as_str)
    };
    assert_eq!(l("init-pro.io/pipeline"), Some("web"), "{path}");
    assert_eq!(l("init-pro.io/canary"), Some("true"), "{path}");
    let rev = l("init-pro.io/revision").filter(|r| !r.is_empty());
    assert_eq!(rev.is_some(), revision, "revision at {path}");
}

/// Weight of backend `service` in a stored `TrafficSplit` (`None` = absent).
fn weight(ts: &Value, service: &str) -> Option<i64> {
    let bs = ts.pointer("/spec/backends")?.as_array()?;
    let hit = bs.iter().find(|b| b["service"].as_str() == Some(service))?;
    hit["weight"].as_i64()
}

/// Both pipeline-owned canary children stored?
async fn canary_children_exist(env: &Env) -> bool {
    let dep = stored(&env.client, &key("apps", "deployments", "web-canary")).await;
    let svc = stored(&env.client, &key("", "services", "web-stable-canary")).await;
    dep.is_object() && svc.is_object()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canary_release_rolls_through_steps_and_promotes() {
    let env = boot_env().await;
    let release = json!({
        "trafficSplit": "web", "stableService": "web-stable",
        "steps": [{"weight": 50, "pauseSeconds": 5}, {"weight": 100, "pauseSeconds": 0}],
    });
    seed(&env, release).await;

    // Step 0 is a 50/50 rewrite of `spec.backends` (Q58 D8), soaked for 5s.
    eventually!(30_000, "the split to reach 50/50", async {
        let ts = stored(&env.client, &key("init-pro.io", "trafficsplits", "web")).await;
        weight(&ts, "web-stable") == Some(50) && weight(&ts, "web-stable-canary") == Some(50)
    });

    // Step 1 (100) soaks its 0s pause, then promotes: phase, tag, weights.
    eventually!(45_000, "the pipeline to promote tag v1", async {
        let sp = stored(&env.client, &key("init-pro.io", "servicepipelines", "web")).await;
        let ts = stored(&env.client, &key("init-pro.io", "trafficsplits", "web")).await;
        sp.pointer("/status/phase").and_then(Value::as_str) == Some("Promoted")
            && sp.pointer("/status/observedTag").and_then(Value::as_str) == Some("v1")
            && weight(&ts, "web-stable") == Some(100)
            && weight(&ts, "web-stable-canary") == Some(0)
    });

    let want = format!("{}/team/web:v1", env.addr);
    let canary = stored(&env.client, &key("apps", "deployments", "web-canary")).await;
    assert!(canary.is_object(), "no canary Deployment: {canary:?}");
    assert_eq!(image_of(&canary), Some(want.as_str()), "canary image");
    assert_eq!(canary["spec"]["replicas"], json!(1), "replicas");
    assert_labels(&canary, "/spec/selector/matchLabels", false);
    assert_labels(&canary, "/spec/template/metadata/labels", true);

    let svc = stored(&env.client, &key("", "services", "web-stable-canary")).await;
    assert!(svc.is_object(), "no canary Service: {svc:?}");
    assert_eq!(svc["spec"]["type"], json!("ClusterIP"), "type");
    assert_labels(&svc, "/spec/selector", true);
    let ports = svc["spec"]["ports"].as_array();
    assert!(ports.is_some_and(|p| !p.is_empty()), "no ports");
    let ports = ports.cloned().unwrap_or_default();
    assert!(ports.iter().all(|p| p["nodePort"].is_null()), "nodePort");

    let stable = stored(&env.client, &key("apps", "deployments", "web")).await;
    assert_eq!(image_of(&stable), Some(want.as_str()), "stable image");
    let ts = stored(&env.client, &key("init-pro.io", "trafficsplits", "web")).await;
    assert_eq!(weight(&ts, "web-stable"), Some(100), "split: {ts:?}");
    assert_eq!(weight(&ts, "web-stable-canary"), Some(0), "split: {ts:?}");
    let sp = stored(&env.client, &key("init-pro.io", "servicepipelines", "web")).await;
    assert_eq!(sp.pointer("/status/rollout"), None, "rollout: {sp:?}");
    assert_eq!(builds(&env.client).await.len(), 1, "one Build");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_release_removes_canary_and_keeps_stable_image() {
    let env = boot_env().await;
    let release = json!({"trafficSplit": "web", "stableService": "web-stable", "abort": true});
    seed(&env, release).await;

    // Abort short-circuits at the top of reconcile, before any Build (D4).
    eventually!(30_000, "the pipeline to abort", async {
        let sp = stored(&env.client, &key("init-pro.io", "servicepipelines", "web")).await;
        sp.pointer("/status/phase").and_then(Value::as_str) == Some("Aborted")
    });

    let kids = canary_children_exist(&env).await;
    assert!(!kids, "abort leaves no canary children");
    let ts = stored(&env.client, &key("init-pro.io", "trafficsplits", "web")).await;
    assert_eq!(weight(&ts, "web-stable"), Some(100), "split: {ts:?}");
    let canary = weight(&ts, "web-stable-canary");
    assert!(matches!(canary, None | Some(0)), "canary weight");
    let stable = stored(&env.client, &key("apps", "deployments", "web")).await;
    assert_eq!(image_of(&stable), Some("team/web:v0"), "stable image");
    let sp = stored(&env.client, &key("init-pro.io", "servicepipelines", "web")).await;
    let tag = sp.pointer("/status/observedTag").and_then(Value::as_str);
    assert_eq!(tag.unwrap_or(""), "", "observedTag");
    assert!(builds(&env.client).await.is_empty(), "no Build");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_the_pipeline_gc_removes_canary_children() {
    let env = boot_env().await;
    let release = json!({
        "trafficSplit": "web", "stableService": "web-stable",
        "steps": [{"weight": 100, "pauseSeconds": 0}],
    });
    seed(&env, release).await;

    eventually!(45_000, "the pipeline to promote its canary", async {
        let sp = stored(&env.client, &key("init-pro.io", "servicepipelines", "web")).await;
        sp.pointer("/status/phase").and_then(Value::as_str) == Some("Promoted")
    });
    // Promotion retains the children at weight 0 for reuse (Q58 D6).
    let kids = canary_children_exist(&env).await;
    assert!(kids, "canary children exist after promote");

    env.client
        .delete(&key("init-pro.io", "servicepipelines", "web"))
        .await
        .unwrap();
    eventually!(30_000, "the canary children to be GC'd", async {
        !canary_children_exist(&env).await
    });

    let dep = env
        .client
        .get(&key("apps", "deployments", "web"))
        .await
        .unwrap();
    assert!(dep.is_some(), "stable Deployment survives GC");
    let svc = env
        .client
        .get(&key("", "services", "web-stable"))
        .await
        .unwrap();
    assert!(svc.is_some(), "stable Service survives GC");
}
