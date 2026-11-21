//! T7.7 (Q63) hardened Agent attempt execution: one engine process per
//! attempt over the sanitized ContainerSpec, per-attempt provider broker,
//! four-way descendant cleanup. Fail-closed: never falls back to the
//! legacy Q37 subprocess path.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use engine_client::{EngineClient, RunStream};
use serde_json::Value;
use tokio::sync::watch;

use crate::agent_broker::{spawn_broker, BrokerHandle};
use crate::agent_exec::{fail_cond, patch_if_changed, Written};
use crate::agent_objects as objs;
use crate::agent_sandbox::{
    open_workspace_to_sandbox, prepare_sandbox_dirs, sandbox_spec, sanitize_attempt_id,
    sanitize_env, select_sandbox, validate_hardened, SandboxInputs, SandboxSelection,
    BROKER_TARGET,
};
use crate::agent_sandbox_plan::{
    attempt_id, audit_denial_count, container_plan, decode_policy, decode_sandbox,
    stage_runner_into_rootfs, write_policy_file,
};
use crate::agents::{AgentRecord, AgentRunnerConfig};
use crate::engine::agent_stream::{wait_verdict, SandboxVerdict};
use crate::engine::launch;
use crate::http::HttpJson;
use api::agent_policy::{denial_audit, effective, EffectivePolicy, ToolClass};

/// Teardown grace before the reap SIGKILL last resort (pump.rs parity).
const REAP_GRACE: Duration = Duration::from_secs(5);
/// Drain budget for the post-signal Exited/Bye frames (best-effort).
const DRAIN_GRACE: Duration = Duration::from_secs(5);
/// Budget for the fresh-connection Bye below (bounded so a wedged engine
/// cannot stall teardown; the reap SIGKILL remains the backstop).
const BYE_GRACE: Duration = Duration::from_secs(1);
/// What one attempt spawned: torn down on EVERY exit path (AS4).
struct Attempt {
    id: String,
    socket: PathBuf,
    child: Option<Child>,
    broker: Option<BrokerHandle>,
}

/// The one shared teardown (AS4). With a live stream and `kill`: signal
/// 9, then Shutdown so the ENGINE runs its graceful stop —
/// `engine/src/session.rs` `stop()` = child.kill + wait + `cleanup()` →
/// `cgroup::remove` writes `cgroup.kill` (`nsrun/src/cgroup.rs`), killing
/// every attempt-cgroup descendant; a wedged engine surviving to the reap
/// SIGKILL still takes the container down via PR_SET_PDEATHSIG
/// (`nsrun/src/child.rs`). After a natural `Exited` the cgroup is already
/// cleaned but the engine CLOSES the session without draining our Shutdown
/// frame — a fresh-connection Shutdown (below) parks it into Bye + exit.
/// Then: reap, revoke the broker, unlink socket.
async fn teardown(attempt: &mut Attempt, stream: Option<&mut RunStream>, kill: bool) {
    if let Some(stream) = stream {
        if kill {
            let _ = stream.send_signal(&attempt.id, 9).await;
        }
        let _ = stream.send_shutdown().await;
        let deadline = tokio::time::Instant::now() + DRAIN_GRACE; // Exited/Bye tail
        while let Ok(Some(_)) = stream.next().await {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
    } else if let Some(child) = attempt.child.as_mut() {
        // No session was ever established: no container to stop gently.
        let _ = child.kill();
    }
    // The run stream dies with its session: when the container exited
    // first (natural, or racing past our Signal 9), `engine/src/session.rs`
    // returns Continue WITHOUT draining our Shutdown frame and closes the
    // socket — the engine would park in its accept loop until the reap
    // SIGKILL a full REAP_GRACE later. Ask once more on a fresh connection
    // (`backend.rs` stop_sandbox_engine parity): a parked engine answers
    // Bye and exits; a busy one takes the request from its backlog.
    if let Some(child) = attempt.child.as_mut() {
        if child.try_wait().ok().flatten().is_none() {
            if let Ok(client) = EngineClient::connect(&attempt.socket).await {
                let _ = tokio::time::timeout(BYE_GRACE, client.shutdown()).await;
            }
        }
    }
    if let Some(mut child) = attempt.child.take() {
        let _ = tokio::task::spawn_blocking(move || launch::reap(&mut child, REAP_GRACE)).await;
    }
    if let Some(broker) = attempt.broker.take() {
        broker.revoke().await;
    }
    let _ = std::fs::remove_file(&attempt.socket);
}

/// Hardened-path branch of [`crate::agent_exec::run_attempt`]. `None` =
/// not a hardened Agent (caller continues the legacy path) or cancelled
/// (same as legacy). `Some((phase, condition))` = terminal/observable
/// outcome. Fail-closed (Q63): every failure returns a condition, never
/// a silent legacy subprocess.
pub(crate) async fn try_hardened(
    client: &HttpJson,
    cfg: &AgentRunnerConfig,
    written: &Written,
    record: &AgentRecord,
    keep_going: &mut watch::Receiver<bool>,
) -> Option<(&'static str, Value)> {
    let view = &record.view;
    let require_hardened = cfg
        .sandbox
        .as_ref()
        .map(|n| n.require_hardened)
        .unwrap_or(false);
    let sandbox = match decode_sandbox(&record.raw) {
        None => None, // legacy Q37 path (unless the node mandates hardening)
        Some(Err(e)) => return Some((objs::PHASE_FAILED, fail_cond("SandboxSpecInvalid", &e))),
        Some(Ok(s)) => Some(s),
    };
    let policy = match decode_policy(&record.raw) {
        Some(Err(e)) => return Some((objs::PHASE_FAILED, fail_cond("PolicySpecInvalid", &e))),
        Some(Ok(p)) => Some(p),
        None => None,
    };
    // T7.7 D7 (Q63): the containment stack = sandbox config + staged
    // runner + staged HARDENED agent-runner (the gated in-process runner).
    let node_ready =
        cfg.sandbox.is_some() && cfg.runner_bin.is_some() && cfg.hardened_runner_bin.is_some();
    if !node_ready {
        tracing::warn!(
            target: "init-pro", agent = %view.key,
            sandbox_config = cfg.sandbox.is_some(),
            runner = cfg.runner_bin.is_some(),
            hardened_agent_runner = cfg.hardened_runner_bin.is_some(),
            "T7.7 containment stack incomplete (Q63); hardened Agents stay Pending"
        );
    }
    let node_cfg = match select_sandbox(sandbox.as_ref(), node_ready, require_hardened) {
        // Node mandates hardening but this Agent has no spec.sandbox:
        // refuse BEFORE the legacy spawn could ever run (T7.7, Q63).
        SandboxSelection::RequiredRefused(msg) => {
            return Some((objs::PHASE_PENDING, fail_cond("SandboxRequired", &msg)))
        }
        // Stay Pending (retried by the loop); NO fallback (Q63).
        SandboxSelection::Invalid(msg) => {
            return Some((objs::PHASE_PENDING, fail_cond("SandboxUnavailable", &msg)))
        }
        SandboxSelection::NotSelected => return None, // defensive: never maps here
        SandboxSelection::Selected => match cfg.sandbox.as_ref() {
            Some(n) => n,
            None => {
                return Some((
                    objs::PHASE_PENDING,
                    fail_cond(
                        "SandboxUnavailable",
                        "node sandbox config missing (Q63); no fallback",
                    ),
                ))
            }
        },
    };
    // `Selected` is only reachable with a decoded spec (the select_sandbox
    // contract); re-bind it for the effective-policy computation below.
    let Some(sandbox) = sandbox else {
        return Some((
            objs::PHASE_PENDING,
            fail_cond(
                "SandboxUnavailable",
                "hardened spec lost after selection (Q63); no fallback",
            ),
        ));
    };
    let attempt = attempt_id(view);
    let eff = match effective(&node_cfg.baseline, policy.as_ref(), &sandbox) {
        Ok(eff) => eff,
        Err(v) => {
            let reason = v.reason();
            tracing::warn!(
                target: "init-pro", agent = %view.key, violation = %reason,
                audit = %denial_audit(&view.key, &attempt, ToolClass::Custom, "spec", &reason),
                "T7.7 policy violation — deterministic deny (Q63)"
            );
            return Some((objs::PHASE_FAILED, fail_cond("PolicyViolation", &reason)));
        }
    };
    // Workspace: the same NFS-subpath work as the legacy path (observable
    // parity of the Workspace* failure shapes).
    let ws_host = match objs::workspace_source(&record.raw, cfg.anyhome_addr.as_deref()) {
        Some((addr, path)) => {
            match crate::agent_exec::mount_workspace(&cfg.data_dir, &addr, &path, &view.name).await
            {
                Ok(ws) => ws,
                Err(e) => {
                    tracing::warn!(target: "init-pro", agent = %view.key, error = %e,
                        "sandbox workspace mount failed");
                    return Some((objs::PHASE_FAILED, fail_cond("WorkspaceMountFailed", &e)));
                }
            }
        }
        None => {
            tracing::warn!(target: "init-pro", agent = %view.key, "node-local workspace");
            crate::agent_exec::workspace_dir(&cfg.data_dir, &view.name)
        }
    };
    if let Err(e) = crate::agent_exec::prepare_workspace(&ws_host).await {
        return Some((objs::PHASE_FAILED, fail_cond("WorkspacePrepareFailed", &e)));
    }
    // T7.7 D7 (Q63): the effective policy rides the workspace at
    // `/workspace/.init-pro/policy.json` (camelCase wire form, Q10); the
    // hardened agent-runner refuses to dispatch without it (fail-closed).
    if let Err(e) = write_policy_file(&ws_host, &eff) {
        tracing::warn!(target: "init-pro", agent = %view.key, error = %e,
            "sandbox policy file write failed");
        return Some((
            objs::PHASE_FAILED,
            fail_cond("PolicyWriteFailed", &e.to_string()),
        ));
    }
    let running = objs::status_patch(
        objs::PHASE_RUNNING,
        &[objs::condition(
            objs::COND_READY,
            "True",
            "Executing",
            "sandboxed runner started",
        )],
    );
    patch_if_changed(client, written, view, &running).await;
    // From here every failure path must tear the attempt down (AS4/AS6).
    let broker = match node_cfg.broker.clone() {
        Some(bcfg) => match spawn_broker(bcfg, &cfg.data_dir, &view.key, &attempt).await {
            Ok(h) => Some(h),
            Err(e) => {
                return Some((
                    objs::PHASE_FAILED,
                    fail_cond("BrokerSpawnFailed", &e.to_string()),
                ))
            }
        },
        None => None,
    };
    let socket = launch::engine_dir(&cfg.data_dir).join(format!("{attempt}.sock"));
    let mut spawned = Attempt {
        id: attempt,
        socket,
        child: None,
        broker,
    };
    run_engine(cfg, &eff, &ws_host, view, &mut spawned, keep_going).await
}

/// Engine spawn + event loop + shared teardown for one attempt. Split off
/// `try_hardened` so each stays readable; same return contract.
async fn run_engine(
    cfg: &AgentRunnerConfig,
    eff: &EffectivePolicy,
    ws_host: &Path,
    view: &objs::AgentView,
    spawned: &mut Attempt,
    keep_going: &mut watch::Receiver<bool>,
) -> Option<(&'static str, Value)> {
    // Shared fail-closed mapping for every pre-session engine failure.
    async fn spawn_failed(spawned: &mut Attempt, msg: String) -> Option<(&'static str, Value)> {
        teardown(spawned, None, false).await;
        Some((objs::PHASE_FAILED, fail_cond("SandboxSpawnFailed", &msg)))
    }
    let (rootfs, scratch) = match prepare_sandbox_dirs(&cfg.data_dir, &spawned.id) {
        Ok(pair) => pair,
        Err(e) => return spawn_failed(spawned, e.to_string()).await,
    };
    // Q63 D7 runner staging: argv[0] is the HARDENED agent-runner (the
    // gated in-process tool registry), staged from
    // `<data>/bin/agent-runner` (or INIT_PRO_AGENT_HARDENED_RUNNER_BIN).
    // `/runner` lives INSIDE the private rootfs (a ro bind is noexec and
    // could never run it; see the fn doc); its parent dir is copied so any
    // sibling payload rides along. The legacy opencode CLI is NOT argv[0].
    let runner_host = cfg.hardened_runner_bin.clone();
    if runner_host.is_none() {
        return spawn_failed(
            spawned,
            "hardened agent-runner not staged (Q63); no fallback".to_string(),
        )
        .await;
    }
    let runner_base = runner_host
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "agent-runner".to_string());
    if let Some(runner) = &runner_host {
        if let Err(e) = stage_runner_into_rootfs(&rootfs, runner) {
            return spawn_failed(spawned, format!("stage hardened agent-runner: {e}")).await;
        }
    }
    let prompt_file = {
        let p = ws_host.join("prompt.md");
        p.is_file().then_some(p)
    };
    // Q63: the workspace is a designated writable surface for the sandbox
    // uid; open it (best-effort, NFS modes are server-side — Q36). The
    // rootfs itself stays root-owned so writes outside workspace/scratch
    // fail for the non-root identity (AS2).
    open_workspace_to_sandbox(ws_host);
    let (argv, plan_envs) = container_plan(view, prompt_file.as_deref(), &runner_base, &spawned.id);
    // Q63: the runner sees the IN-CONTAINER broker endpoint, never the
    // host path (invisible inside the private mount namespace).
    let env = sanitize_env(
        &plan_envs,
        spawned.broker.as_ref().map(|_| Path::new(BROKER_TARGET)),
    );
    let spec = sandbox_spec(&SandboxInputs {
        attempt_id: spawned.id.clone(),
        rootfs,
        eff: eff.clone(),
        workspace_host: ws_host.to_path_buf(),
        scratch_host: scratch,
        runner_host,
        broker_socket: spawned.broker.as_ref().map(|b| b.socket_path.clone()),
        argv,
        env,
        hostname: Some(sanitize_attempt_id(&view.name)),
    });
    if let Err(e) = validate_hardened(&spec) {
        teardown(spawned, None, false).await;
        return Some((objs::PHASE_FAILED, fail_cond("SandboxSpecInvalid", &e)));
    }
    // Stale socket from a previous run of the same object id: best-effort
    // unlink (spawn_engine does not; the broker does its own).
    let _ = std::fs::remove_file(&spawned.socket);
    match launch::spawn_engine(&spawned.socket, false, false) {
        Ok(child) => spawned.child = Some(child),
        Err(e) => return spawn_failed(spawned, e.to_string()).await,
    }
    if let Err(e) = launch::wait_socket(&spawned.socket).await {
        return spawn_failed(spawned, e.to_string()).await;
    }
    let client = match EngineClient::connect(&spawned.socket).await {
        Ok(c) => c,
        Err(e) => return spawn_failed(spawned, e.to_string()).await,
    };
    let mut stream = match client.run(spec).await {
        Ok(s) => s,
        Err(e) => return spawn_failed(spawned, e.to_string()).await,
    };
    let (verdict, stderr) =
        wait_verdict(&mut stream, &spawned.id, cfg.exec_timeout, keep_going).await;
    let natural = matches!(verdict, SandboxVerdict::Exited(_));
    teardown(spawned, Some(&mut stream), !natural).await;
    let session_note = objs::session_id_from_stderr(&stderr)
        .map(|s| format!("; session {s}"))
        .unwrap_or_default();
    // T7.7 D7 (Q63): advisory denial count from the in-workspace audit
    // JSONL — agent-tamperable by design, so it never influences the
    // verdict, only observability. Read after teardown (the runner may
    // flush its last line at exit).
    let attempt = spawned.id.clone();
    let audit_note = match audit_denial_count(ws_host, &attempt) {
        Some(n) if n > 0 => {
            tracing::warn!(
                target: "init-pro", attempt = %attempt, denials = n,
                "T7.7 gate denials recorded in the workspace audit (advisory, Q63)"
            );
            format!(" (audit: {n} denials [advisory])")
        }
        _ => String::new(),
    };
    match verdict {
        SandboxVerdict::Cancelled => None, // deleted mid-flight
        SandboxVerdict::Timeout => Some((
            objs::PHASE_FAILED,
            fail_cond("ExecTimeout", "attempt exceeded budget"),
        )),
        SandboxVerdict::Error(msg) => Some((
            objs::PHASE_FAILED,
            fail_cond("RunnerFailed", &format!("engine error: {msg}")),
        )),
        SandboxVerdict::Exited(0) => Some((
            objs::PHASE_SUCCEEDED,
            objs::condition(
                objs::COND_READY,
                "True",
                "Completed",
                &format!("exit 0{session_note}{audit_note}"),
            ),
        )),
        SandboxVerdict::Exited(code) => Some((
            objs::PHASE_FAILED,
            fail_cond(
                "RunnerFailed",
                &format!("exit {code}{session_note}{audit_note}"),
            ),
        )),
    }
}
