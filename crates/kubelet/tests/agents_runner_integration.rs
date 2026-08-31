//! `Agent` runner integration tests (TODO **T7.1** Phase B, decision
//! **Q37**) over a REAL apiserver: the full node-side loop from
//! `kubelet::spawn_agent_runner` (LIST + watch, execution attempt, status
//! merge-patch write-back) against a stub `opencoder` binary. Covers the
//! happy path (Pending -> Running -> Succeeded + session id + workspace
//! scaffold), the `RunnerMissing` degradation, nonzero exits
//! (`RunnerFailed`), `ExecTimeout`, and delete-cascade (the subprocess is
//! killed). Workspace mode = node-local fallback (no NFS in unit CI).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::SchemaRegistry;
use kubelet::{spawn_agent_runner, AgentRunnerConfig, HttpJson};
use serde_json::{json, Value};
use storage::{EmbeddedStorage, StorageBackend};
use tokio::net::TcpListener;

const NODE: &str = "agent-it-node";

// ---- server + helpers ----------------------------------------------------

fn served() -> SchemaRegistry {
    let mut reg = SchemaRegistry::with_core_v1();
    api::initpro::register(&mut reg);
    api::agent::register(&mut reg); // the generic routes serve `agents` as-is
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

fn data_dir() -> PathBuf {
    let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("init-pro-agent-it-{}-{n}", std::process::id()))
}

/// A stub `opencoder` at the staged location (`<data>/bin/opencoder`).
fn write_runner(data: &Path, body: &str) -> PathBuf {
    let bin = data.join("bin/opencoder");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn agent_object(name: &str, prompt: &str) -> Value {
    json!({
        "apiVersion": "init-pro.io/v1", "kind": "Agent",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {"prompt": prompt, "nodeName": NODE},
        "status": {"phase": "Pending"}
    })
}

async fn post_agent(client: &HttpJson, name: &str, prompt: &str) {
    let (code, _) = client
        .post_json(
            "/apis/init-pro.io/v1/namespaces/default/agents",
            &agent_object(name, prompt),
        )
        .await
        .unwrap();
    assert_eq!(code, 201, "agent POST rejected");
}

async fn post_agent_on(
    client: &HttpJson,
    name: &str,
    prompt: &str,
    node: &str,
) {
    let mut obj = agent_object(name, prompt);
    obj["spec"]["nodeName"] = json!(node);
    let (code, _) = client
        .post_json("/apis/init-pro.io/v1/namespaces/default/agents", &obj)
        .await
        .unwrap();
    assert_eq!(code, 201, "agent POST rejected");
}

fn cfg_of(url: &str, data: &Path, runner: Option<PathBuf>, timeout: Duration) -> AgentRunnerConfig {
    let mut cfg = AgentRunnerConfig::new(url.to_string(), NODE.to_string(), data.to_path_buf());
    cfg.runner_bin = runner;
    cfg.anyhome_addr = None; // node-local workspace fallback (no NFS here)
    cfg.exec_timeout = timeout;
    cfg
}

async fn wait_for(client: &HttpJson, path: &str, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok((200, v)) = client.get_json(path).await {
            if pred(&v) {
                return v;
            }
        }
        assert!(Instant::now() < deadline, "timeout waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn condition_of(v: &Value, reason_want: &str) -> bool {
    v["status"]["conditions"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .any(|c| c["type"] == "Ready" && c["reason"] == reason_want)
        })
        .unwrap_or(false)
}

// ---- tests ----------------------------------------------------------------

#[tokio::test]
async fn pending_running_succeeded_with_session_and_local_workspace() {
    let data = data_dir();
    let runner = write_runner(
        &data,
        &format!(
            "printf '%s\\n' \"$@\" > {0}/last-args\necho \"[session s-123]\" >&2\nexit 0\n",
            data.display()
        ),
    );
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let shutdown = infra::Shutdown::new();
    let _handles = spawn_agent_runner(cfg_of(&url, &data, Some(runner), Duration::from_secs(60)), shutdown.clone());

    post_agent(&client, "ok-agent", "task one").await;
    let path = "/apis/init-pro.io/v1/namespaces/default/agents/ok-agent";
    let done = wait_for(&client, path, "Succeeded agent", |v| {
        v["status"]["phase"] == "Succeeded" && condition_of(v, "Completed")
    })
    .await;
    assert!(done["status"]["conditions"][0]["message"]
        .as_str()
        .unwrap()
        .contains("session s-123"));

    // The task prompt rode as the positional arg (persona file absent).
    let args = std::fs::read_to_string(data.join("last-args")).unwrap();
    assert!(args.contains("--workdir"));
    assert!(args.contains("agent-ws/ok-agent"));
    assert!(args.contains("task one"));
    assert!(!args.contains("--prompt-file"));

    // The node-local scaffold mirrors the keeper layout (Q37).
    let ws = data.join("agent-ws/ok-agent");
    assert!(ws.join("mem").is_dir());
    let bridge = ws.join(".opencoder/skills");
    assert!(
        bridge.read_link().is_ok(),
        ".opencoder/skills should be a symlink"
    );

    shutdown.trigger();
}

#[tokio::test]
async fn missing_runner_stays_pending_with_runnermissing_condition() {
    let data = data_dir();
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let shutdown = infra::Shutdown::new();
    let _handles = spawn_agent_runner(cfg_of(&url, &data, None, Duration::from_secs(60)), shutdown.clone());

    post_agent(&client, "no-runner", "task two").await;
    let path = "/apis/init-pro.io/v1/namespaces/default/agents/no-runner";
    let v = wait_for(&client, path, "RunnerMissing condition", |v| {
        v["status"]["phase"] == "Pending" && condition_of(v, "RunnerMissing")
    })
    .await;
    assert_eq!(v["status"]["conditions"][0]["status"], "False");
    shutdown.trigger();
}

#[tokio::test]
async fn nonzero_exit_fails_with_runnerfailed_reason() {
    let data = data_dir();
    let runner = write_runner(&data, "echo \"[session s-9]\" >&2\nexit 7\n");
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let shutdown = infra::Shutdown::new();
    let _handles = spawn_agent_runner(cfg_of(&url, &data, Some(runner), Duration::from_secs(60)), shutdown.clone());

    post_agent(&client, "bad-exit", "task three").await;
    let path = "/apis/init-pro.io/v1/namespaces/default/agents/bad-exit";
    let v = wait_for(&client, path, "Failed agent", |v| {
        v["status"]["phase"] == "Failed" && condition_of(v, "RunnerFailed")
    })
    .await;
    let msg = v["status"]["conditions"][0]["message"].as_str().unwrap();
    assert!(msg.contains("exit 7"), "message should carry the exit code: {msg}");
    assert!(msg.contains("session s-9"), "message should carry the session id: {msg}");
    shutdown.trigger();
}

#[tokio::test]
async fn hung_runner_is_killed_and_marked_exectimeout() {
    let data = data_dir();
    let runner = write_runner(&data, "export PATH=/bin:/usr/bin\nsleep 30\n");
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let shutdown = infra::Shutdown::new();
    let _handles = spawn_agent_runner(cfg_of(&url, &data, Some(runner), Duration::from_millis(300)), shutdown.clone());

    post_agent(&client, "hung", "task four").await;
    let path = "/apis/init-pro.io/v1/namespaces/default/agents/hung";
    let v = wait_for(&client, path, "ExecTimeout failure", |v| {
        v["status"]["phase"] == "Failed" && condition_of(v, "ExecTimeout")
    })
    .await;
    assert_eq!(v["status"]["conditions"][0]["status"], "False");
    shutdown.trigger();
}

#[tokio::test]
async fn deleting_a_running_agent_kills_the_subprocess() {
    let data = data_dir();
    let pid_file = data.join("child-pid");
    let runner = write_runner(
        &data,
        &format!(
            "echo $$ > {}\nexport PATH=/bin:/usr/bin\nsleep 30\n",
            pid_file.display()
        ),
    );
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let shutdown = infra::Shutdown::new();
    let _handles = spawn_agent_runner(cfg_of(&url, &data, Some(runner), Duration::from_secs(60)), shutdown.clone());

    post_agent(&client, "deleted-mid-run", "task five").await;
    let path = "/apis/init-pro.io/v1/namespaces/default/agents/deleted-mid-run";
    wait_for(&client, path, "Running agent", |v| {
        v["status"]["phase"] == "Running" && condition_of(v, "Executing")
    })
    .await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if pid_file.exists() {
            break;
        }
        assert!(Instant::now() < deadline, "stub never wrote its pid");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = std::fs::read_to_string(&pid_file).unwrap().trim().to_string();

    // DELETE cascade: watch DELETED -> keep_going=false -> kill_on_drop.
    let (code, _) = client.delete_json(path).await.unwrap();
    assert_eq!(code, 200);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            break;
        }
        assert!(Instant::now() < deadline, "subprocess {pid} survived DELETE");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown.trigger();
}

// Binding filter regression (Q37): an agent bound to ANOTHER node must not
// enter this node's desired map — not via LIST and, critically, not via a
// watch ADDED event (an unfiltered watch insert let node A execute node B's
// agent before B could report its own condition). Node A has a runner and
// must stay silent; node B has none and must surface RunnerMissing.
#[tokio::test]
async fn foreign_bound_agent_is_never_executed_locally() {
    let url = spawn_server().await;
    let client = HttpJson::parse_url(&url).unwrap();
    let shutdown = infra::Shutdown::new();
    let data_a = data_dir();
    let data_b = data_dir();
    let runner = write_runner(
        &data_a,
        "echo 'exit 0; session stolen-s' > \"$INIT_PRO_SESSION_FILE\"\nexit 0\n",
    );
    let _handles = spawn_agent_runner(
        cfg_of(&url, &data_a, Some(runner), Duration::from_secs(60)),
        shutdown.clone(),
    );
    // Node B: same apiserver, runnerless — the legitimate owner of the agent.
    let mut cfg_b = cfg_of(&url, &data_b, None, Duration::from_secs(60));
    cfg_b.node_name = "agent-it-other".to_string();
    let _handles_b = spawn_agent_runner(cfg_b, shutdown.clone());

    post_agent_on(&client, "foreign-bound", "noprompt", "agent-it-other").await;
    let v = wait_for(
        &client,
        "/apis/init-pro.io/v1/namespaces/default/agents/foreign-bound",
        "RunnerMissing on the bound node",
        |v| condition_of(v, "RunnerMissing"),
    )
    .await;
    assert_eq!(v["status"]["phase"], "Pending", "body: {v}");
    // Give the thief a full watch window to prove it stayed silent.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (_, after) = client
        .get_json("/apis/init-pro.io/v1/namespaces/default/agents/foreign-bound")
        .await
        .unwrap();
    assert_eq!(after["status"]["phase"], "Pending", "stolen by other node: {after}");
}
