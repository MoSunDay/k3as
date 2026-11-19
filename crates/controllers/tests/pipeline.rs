//! T8.3 acceptance (decision **Q56**): in-process end to end through the
//! real `ControllerManager` (leader election + informers + workqueues +
//! reconciler workers), a REAL embedded registry served over loopback and a
//! fake `docker` CLI shim.
//!
//! Covered flow: `ServicePipeline` -> `Build` child -> fake `docker build` /
//! `docker save` -> `registry::push_image` -> `Build` `Succeeded` -> target
//! `Deployment` image patch + pipeline `Promoted`.
//!
//! Every wait is a poll with timeout -- NO fixed timing windows.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use controllers::{Client, ControllerManager, ManagerConfig, PipelineConfig, Stop, StorageClient};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, Key, KeyPrefix};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

/// Poll `cond` (an async block) until true, panicking after `ms`.
macro_rules! eventually {
    ($ms:expr, $cond:expr) => {{
        let deadline = Instant::now() + Duration::from_millis($ms);
        loop {
            if $cond.await {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the pipeline to converge"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }};
}

/// Per-binary fixture-dir sequence: pid + millis alone collide when two tests
/// boot within the same millisecond (one dir would delete the other's).
static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_tmp_dir() -> PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "init-pro-pipeline-e2e-{}-{}-{seq}",
        std::process::id(),
        registry::now_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Live harness: the manager handles + `stop` stay alive for the whole test
/// (mirrors `tests/controllers.rs`), and the fixture dir is reaped on drop
/// (best effort, also on assertion failure).
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
        cleanup_dir(&self.tmp);
    }
}

/// Best-effort fixture-dir reaping. `/tmp` here is ZFS, which can
/// transiently answer `rmdir` with `ENOTEMPTY` for a tree nothing is
/// writing to (every entry's mtime precedes the teardown), so retry a few
/// times and never fail the test on cleanup.
fn cleanup_dir(path: &Path) {
    for _ in 0..10 {
        if std::fs::remove_dir_all(path).is_ok() || !path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
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

fn append_tar_file(builder: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    builder.append_data(&mut header, name, data).unwrap();
}

/// A plain (uncompressed) `docker save` archive, as the shim produces.
fn write_fixture(tmp: &Path) -> PathBuf {
    let manifest = br#"[{"Config":"cfg.json","RepoTags":["team/web:v1"],"Layers":["layer.tar"]}]"#;
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let layer = b"layer bytes for the pipeline e2e\n";
    let mut builder = tar::Builder::new(Vec::new());
    append_tar_file(&mut builder, "manifest.json", manifest);
    append_tar_file(&mut builder, "cfg.json", config);
    append_tar_file(&mut builder, "layer.tar", layer);
    let path = tmp.join("fixture.tar");
    std::fs::write(&path, builder.into_inner().unwrap()).unwrap();
    path
}

/// Fake `docker`: `build` succeeds, `save -o <tar> <ref>` copies the fixture
/// to `$3` (the controller's output path).
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
    let handles = ControllerManager::spawn_with(
        store.clone(),
        stop.clone(),
        ManagerConfig {
            pipeline: Some(PipelineConfig {
                registry_addr: addr.clone(),
                scratch_dir: tmp.join("scratch"),
                docker_bin,
                default_timeout_seconds: 30,
            }),
            ..Default::default()
        },
    );
    let client = StorageClient::new(store);
    Env {
        client,
        addr,
        tmp,
        _stop: stop,
        _handles: handles,
    }
}

fn sp_key() -> Key {
    Key::new("init-pro.io", "servicepipelines", "default", "web")
}

fn dep_key() -> Key {
    Key::new("apps", "deployments", "default", "web")
}

async fn stored(client: &StorageClient, key: &Key) -> Value {
    client.get(key).await.unwrap().unwrap_or(Value::Null)
}

async fn builds(client: &StorageClient) -> Vec<Value> {
    client
        .list(&KeyPrefix::new(
            "init-pro.io",
            "builds",
            Some("default".to_string()),
        ))
        .await
        .unwrap()
}

fn deployment() -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "replicas": 1,
            "selector": {"matchLabels": {"app": "web"}},
            "template": {
                "metadata": {"labels": {"app": "web"}},
                "spec": {"containers": [{"name": "app", "image": "team/web:v0"}]},
            },
        },
    })
}

/// The promoted repository carries the registry host so the image the
/// Deployment ends up with is pullable as-is (`<addr>/team/web:v1`).
fn pipeline(addr: &str, context: &Path) -> Value {
    json!({
        "apiVersion": "init-pro.io/v1",
        "kind": "ServicePipeline",
        "metadata": {"name": "web", "namespace": "default"},
        "spec": {
            "image": {"repository": format!("{addr}/team/web"), "tag": "v1"},
            "source": {"context": context.display().to_string()},
            "target": {"deployment": "web", "container": "app"},
            "build": {"timeoutSeconds": 30},
        },
    })
}

fn image_of(dep: &Value) -> Option<&str> {
    dep.pointer("/spec/template/spec/containers/0/image")
        .and_then(Value::as_str)
}

/// Minimal HTTP/1.1 GET over a real socket (Content-Length framing); the
/// connect is retried -- loopback can answer SYN with RST under load.
async fn http_get(addr: &str, path: &str) -> (u16, String) {
    let mut stream = None;
    for _ in 0..50 {
        match TcpStream::connect(addr).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let mut stream = stream.unwrap_or_else(|| panic!("connect {addr}"));
    let head = format!(
        "GET {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let hend = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("HTTP head terminator");
    let head = String::from_utf8_lossy(&raw[..hend]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .expect("HTTP status line");
    (
        status,
        String::from_utf8_lossy(&raw[hend + 4..]).to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builds_publishes_and_promotes_e2e() {
    let env = boot_env().await;
    let src = env.tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    env.client.create(&dep_key(), deployment()).await.unwrap();
    env.client
        .create(&sp_key(), pipeline(&env.addr, &src))
        .await
        .unwrap();

    // Pipeline -> Build -> build/save/push -> promotion, end to end. The
    // Build must be Succeeded AND the pipeline must have observed the tag.
    eventually!(30000, async {
        let builds = builds(&env.client).await;
        if builds.len() != 1 {
            return false;
        }
        if builds[0].pointer("/status/phase").and_then(Value::as_str) != Some("Succeeded") {
            return false;
        }
        let sp = stored(&env.client, &sp_key()).await;
        sp.pointer("/status/phase").and_then(Value::as_str) == Some("Promoted")
            && sp.pointer("/status/observedTag").and_then(Value::as_str) == Some("v1")
    });

    let builds = builds(&env.client).await;
    assert_eq!(builds.len(), 1, "one Build per (pipeline, tag): {builds:?}");
    let build = &builds[0];
    let digest = build
        .pointer("/status/digest")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(digest.len(), 71, "digest {digest:?}");
    assert!(digest.starts_with("sha256:"), "digest {digest:?}");
    let log = build
        .pointer("/status/logTail")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(log.contains("fake docker build ok"), "logTail {log:?}");
    assert!(log.contains("fake docker save ok"), "logTail {log:?}");

    // The promoted image is really in the registry (raw V2 API).
    let (status, body) = http_get(&env.addr, "/v2/team/web/tags/list").await;
    assert_eq!(status, 200, "tags/list body {body:?}");
    let tags: Value = serde_json::from_str(&body).expect("tags/list JSON");
    let tags = tags["tags"].as_array().cloned().unwrap_or_default();
    assert!(tags.contains(&json!("v1")), "registry tags {tags:?}");

    // Promotion patched the Deployment's container image...
    let dep = stored(&env.client, &dep_key()).await;
    let want = format!("{}/team/web:v1", env.addr);
    assert_eq!(image_of(&dep), Some(want.as_str()), "deployment image");

    // ...and the pipeline status points back at the Build that produced it.
    let sp = stored(&env.client, &sp_key()).await;
    assert_eq!(
        sp.pointer("/status/lastBuild/name").and_then(Value::as_str),
        build.pointer("/metadata/name").and_then(Value::as_str),
    );
}

/// Q56 disabled posture: with `ManagerConfig::pipeline == None` the plane has
/// no informers and no workers, so a `ServicePipeline` produces no Build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_pipeline_plane_creates_no_build() {
    let tmp = unique_tmp_dir();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let store = Arc::new(EmbeddedStorage::new());
    let stop = Stop::new();
    let _handles =
        ControllerManager::spawn_with(store.clone(), stop.clone(), ManagerConfig::default());
    let client = StorageClient::new(store);
    client.create(&dep_key(), deployment()).await.unwrap();
    client
        .create(&sp_key(), pipeline("127.0.0.1:15000", &src))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        builds(&client).await.is_empty(),
        "no Build when the plane is off"
    );
    let sp = stored(&client, &sp_key()).await;
    assert_eq!(
        sp.pointer("/status/phase"),
        None,
        "status untouched: {sp:?}"
    );
    cleanup_dir(&tmp);
}
