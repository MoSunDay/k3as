//! Node-side `Agent` execution loops (TODO **T7.1** Phase B / B1,
//! decisions **Q35** + **Q37**).
//!
//! Mirrors the pod loops in [`crate::runner`]: LIST + WATCH
//! `/apis/init-pro.io/v1/agents` with CLIENT-SIDE `spec.nodeName` filtering
//! (the apiserver ignores ListParams selectors -- G52), then per bound
//! Agent one execution attempt ([`crate::agent_exec::run_attempt`]: anyhome
//! workspace, staged `opencoder` subprocess per **D1**, merge-patch status
//! write-back -- agents have no `/status` subresource).
//!
//! Semantics (Q37): at-least-once. A runner restart finds an orphan
//! `Running` Agent, resets it to `Pending` (reason `Restarted`) and
//! re-executes -- tasks MUST be idempotent. A missing runner binary keeps
//! Agents `Pending` with `Ready=False, reason=RunnerMissing` (observable,
//! never fatal); deletion kills the subprocess and unmounts the workspace.
//!
//! Long-run hardening (C14): an Agent that stays Pending (`SandboxRequired`
//! / `SandboxUnavailable`, H2) is retried with a bounded exponential
//! backoff instead of every scan; the `killers` slot of an attempt is a
//! drop guard (H3) so a panicking attempt cannot pin it forever; the
//! per-attempt sandbox dirs of a crashed kubelet are swept at boot (H4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{watch, Mutex};

use crate::agent_exec::{dedupe_id, fail_cond, patch_if_changed, run_attempt, Written};
use crate::agent_objects as objs;
use crate::agent_sandbox::sweep_stale_attempt_dirs;
use crate::agent_sandbox_plan::retry_delay;
use crate::http::HttpJson;

/// One execution attempt budget (Q37 fault surface); overridable via
/// `INIT_PRO_AGENT_EXEC_TIMEOUT_SECS` (tests/e2e knob).
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(3600);
/// How often the run loop scans the desired map.
const RUN_PERIOD: Duration = Duration::from_millis(500);
/// The agent collection path (all namespaces; filtered client-side).
const AGENTS_PATH: &str = "/apis/init-pro.io/v1/agents";

/// Configuration for the agent runner (built by the `agent` CLI wiring).
#[derive(Debug, Clone)]
pub struct AgentRunnerConfig {
    pub server_url: String,
    pub node_name: String,
    pub data_dir: PathBuf,
    /// Staged runner binary; `None` => `RunnerMissing` degradation.
    pub runner_bin: Option<PathBuf>,
    /// Staged HARDENED agent-runner binary (T7.7 D7, Q63) — the in-process
    /// gated runner executed inside the sandbox. `None` => hardened Agents
    /// stay Pending `SandboxUnavailable` (fail-closed; never the legacy
    /// subprocess and never an ungated runner).
    pub hardened_runner_bin: Option<PathBuf>,
    /// Fallback anyhome NFS address when an Agent carries no workspace
    /// annotation (`--anyhome-addr`; the single-node default needs no flag).
    pub anyhome_addr: Option<String>,
    /// One attempt budget (`INIT_PRO_AGENT_EXEC_TIMEOUT_SECS`).
    pub exec_timeout: Duration,
    /// T7.7 (Q63) node-side sandbox config; `None` keeps hardened Agents
    /// Pending (fail-closed) — never the legacy subprocess path.
    pub sandbox: Option<crate::agent_sandbox::SandboxNodeConfig>,
    /// Static apiserver bearer token (T1.3a, Q47 P3): the Agent watch and
    /// status merge-patches authenticate like the kubelet loops.
    pub server_token: Option<String>,
}

impl AgentRunnerConfig {
    pub fn new(server_url: String, node_name: String, data_dir: PathBuf) -> Self {
        Self {
            server_url,
            node_name,
            data_dir,
            runner_bin: None,
            hardened_runner_bin: None,
            anyhome_addr: None,
            exec_timeout: exec_timeout_from_env().unwrap_or(DEFAULT_EXEC_TIMEOUT),
            server_token: None,
            sandbox: None,
        }
    }
}

/// `INIT_PRO_AGENT_EXEC_TIMEOUT_SECS` (invalid values fall back to default).
pub(crate) fn exec_timeout_from_env() -> Option<Duration> {
    std::env::var("INIT_PRO_AGENT_EXEC_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// What the run loop needs per Agent: the extracted view plus the raw
/// object (the workspace annotation is metadata-only, not in the view).
#[derive(Debug, Clone)]
pub(crate) struct AgentRecord {
    pub view: objs::AgentView,
    pub raw: Value,
}

type Desired = Arc<Mutex<BTreeMap<String, AgentRecord>>>;
/// Per-Agent kill switches: `true` = keep going, `false` = die now (also
/// guards an in-flight orphan reset so it patches once, not per beat).
type Killers = Arc<Mutex<BTreeMap<String, watch::Sender<bool>>>>;
/// H2 (C14): per-key Pending backoff of the run loop (keyed by
/// `namespace/name`; see [`PendingBackoff`] for the reset rules).
type Backoff = Arc<Mutex<BTreeMap<String, PendingBackoff>>>;

/// H2 (C14): consecutive Pending outcomes of one Agent and when to retry.
/// `object` is the object stamp ([`object_stamp`]: uid + verbatim spec)
/// the failures were counted for: a recreated OR spec-edited object starts
/// over (an operator fix must never wait out a 5 min cap), while the
/// runner's own STATUS writes keep it (a resourceVersion stamp would be
/// bumped by every such write and reset the curve each round).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingBackoff {
    object: String,
    failures: u32,
    next_retry: Instant,
}

/// Bounded spin on `try_lock` for the drop path (H3): every holder of the
/// `killers` lock in this module drops it before any `.await`.
const SLOT_SPINS: usize = 16;

/// Pure: the identity the backoff is counted for -- the uid plus the
/// verbatim `spec` (canonical JSON: serde_json sorts keys here, so LIST
/// and WATCH render the same bytes for the same stored spec).
fn object_stamp(raw: &Value) -> String {
    let uid = raw
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    let spec = raw
        .pointer("/spec")
        .map(Value::to_string)
        .unwrap_or_default();
    format!("{uid}#{spec}")
}

/// Pure: whether the loop must skip this Pending Agent right now.
fn backoff_active(entry: Option<&PendingBackoff>, stamp: &str, now: Instant) -> bool {
    match entry {
        Some(b) => b.object == stamp && now < b.next_retry,
        None => false,
    }
}

/// Pure: the state after one more consecutive Pending outcome (`prev`
/// counts only when it was recorded for the same object stamp).
fn backoff_after_failure(
    prev: Option<&PendingBackoff>,
    stamp: &str,
    now: Instant,
) -> PendingBackoff {
    let failures = match prev {
        Some(b) if b.object == stamp => b.failures.saturating_add(1),
        _ => 1,
    };
    PendingBackoff {
        object: stamp.to_string(),
        failures,
        next_retry: now + retry_delay(failures),
    }
}

/// H3 (C14): the `killers` slot lease of one attempt. `run_task` inserts
/// the slot and hands the guard to the attempt task; `Drop` removes the
/// key on EVERY exit -- return, panic unwind, task cancellation -- so a
/// panicking `run_attempt` can no longer pin the Agent out of the scan
/// (the slot is exactly what keeps the loop from re-spawning it).
struct SlotGuard {
    killers: Killers,
    key: String,
}

impl SlotGuard {
    /// Insert the slot; returns the guard plus its kill-switch receiver.
    async fn acquire(killers: Killers, key: &str) -> (Self, watch::Receiver<bool>) {
        let (tx, rx) = watch::channel(true);
        killers.lock().await.insert(key.to_string(), tx);
        (
            Self {
                killers,
                key: key.to_string(),
            },
            rx,
        )
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if !release_slot(&self.killers, &self.key) {
            tracing::debug!(target: "init-pro", agent = %self.key,
                "killers slot release deferred to the runtime (C14)");
        }
    }
}

/// Synchronous best-effort removal for the drop path: a short `try_lock`
/// spin, then a detached async removal when a runtime is live (a `Drop`
/// cannot await). `true` = removed inline.
fn release_slot(killers: &Killers, key: &str) -> bool {
    for _ in 0..SLOT_SPINS {
        if let Ok(mut slots) = killers.try_lock() {
            slots.remove(key);
            return true;
        }
        std::thread::yield_now();
    }
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let (killers, key) = (Arc::clone(killers), key.to_string());
        runtime.spawn(async move {
            killers.lock().await.remove(&key);
        });
    }
    false
}

/// Spawn the watch + run loops (same shape as [`crate::runner::spawn`]).
pub fn spawn_agent_runner(
    cfg: AgentRunnerConfig,
    shutdown: infra::Shutdown,
) -> Vec<tokio::task::JoinHandle<()>> {
    let client = match HttpJson::parse_url(&cfg.server_url) {
        Ok(c) => Arc::new(c.with_token(cfg.server_token.clone())),
        Err(e) => {
            tracing::error!(target: "init-pro", error = %e, "agent runner: bad server URL; not starting");
            return Vec::new();
        }
    };
    tracing::info!(
        target: "init-pro",
        node = %cfg.node_name,
        runner = ?cfg.runner_bin.as_ref().map(|p| p.display().to_string()),
        "agent runner starting (T7.1 Phase B, Q37)"
    );
    let desired: Desired = Arc::new(Mutex::new(BTreeMap::new()));
    let killers: Killers = Arc::new(Mutex::new(BTreeMap::new()));
    let written: Written = Arc::new(Mutex::new(BTreeMap::new()));
    // H4 (C14): boot sweep BEFORE the loops start, so no attempt of this
    // boot can race it (bounded: only `<data>/agent-sandbox/<id>` trees
    // whose engine socket is gone; the data dir is small at boot).
    let swept = sweep_stale_attempt_dirs(&cfg.data_dir);
    if swept > 0 {
        tracing::info!(target: "init-pro", swept,
            "agent runner: removed stale sandbox attempt dirs (C14)");
    }
    vec![
        tokio::spawn(watch_task(
            client.clone(),
            cfg.node_name.clone(),
            desired.clone(),
            killers.clone(),
            shutdown.clone(),
        )),
        tokio::spawn(run_task(
            client,
            Arc::new(cfg),
            desired,
            killers,
            written,
            shutdown,
        )),
    ]
}

/// Task 1: LIST + watch the agent collection (client-side nodeName filter).
async fn watch_task(
    client: Arc<HttpJson>,
    node_name: String,
    desired: Desired,
    killers: Killers,
    shutdown: infra::Shutdown,
) {
    loop {
        if shutdown.is_triggered() {
            return;
        }
        if let Err(e) = watch_cycle(&client, &node_name, &desired, &killers, &shutdown).await {
            if shutdown.is_triggered() {
                return;
            }
            tracing::debug!(target: "init-pro", error = %e, "agent runner: watch cycle ended; re-LIST in 500ms");
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

async fn watch_cycle(
    client: &HttpJson,
    node_name: &str,
    desired: &Desired,
    killers: &Killers,
    shutdown: &infra::Shutdown,
) -> Result<(), String> {
    let (code, list) = client
        .get_json(AGENTS_PATH)
        .await
        .map_err(|e| e.to_string())?;
    if code != 200 {
        return Err(format!("agent LIST returned {code}"));
    }
    let mut map = BTreeMap::new();
    for item in list
        .pointer("/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if objs::agent_node(item).as_deref() != Some(node_name) {
            continue;
        }
        if let Some(view) = objs::agent_view(item) {
            let key = view.key.clone();
            map.insert(
                key,
                AgentRecord {
                    view,
                    raw: item.clone(),
                },
            );
        }
    }
    // A DELETE missed during a watch gap still kills an in-flight execution.
    let live: BTreeSet<String> = map.keys().cloned().collect();
    let stale: Vec<String> = killers
        .lock()
        .await
        .keys()
        .filter(|k| !live.contains(*k))
        .cloned()
        .collect();
    for key in stale {
        if let Some(tx) = killers.lock().await.remove(&key) {
            let _ = tx.send(false);
        }
    }
    *desired.lock().await = map;
    let rv = list
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .unwrap_or("0");
    let mut conn = client
        .watch(&format!("{AGENTS_PATH}?watch=1&resourceVersion={rv}"))
        .await
        .map_err(|e| e.to_string())?;
    loop {
        let line = tokio::select! {
            _ = shutdown.cancelled() => return Err("shutdown".into()),
            line = conn.next_line() => line,
        };
        let Some(line) = line else {
            return Err("watch stream ended".into());
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let typ = ev.pointer("/type").and_then(Value::as_str).unwrap_or("");
        let obj = ev.pointer("/object").cloned().unwrap_or(Value::Null);
        let Some(key) = objs::agent_key(&obj) else {
            continue;
        };
        // Same client-side binding filter as the LIST path: watch events
        // for agents bound to OTHER nodes (or unbound) must never enter
        // this node's desired map (Q37; an unfiltered insert let a foreign
        // node execute a bound agent in the e2e).
        if objs::agent_node(&obj).as_deref() != Some(node_name) {
            continue;
        }
        if typ == "DELETED" {
            desired.lock().await.remove(&key);
            if let Some(tx) = killers.lock().await.remove(&key) {
                let _ = tx.send(false); // kill the subprocess, if any
            }
            continue;
        }
        if let Some(view) = objs::agent_view(&obj) {
            desired
                .lock()
                .await
                .insert(key, AgentRecord { view, raw: obj });
        }
    }
}

/// Task 2: scan the desired map, drive every Pending Agent through one
/// execution attempt (unbounded concurrency: one OS process per Agent,
/// disjoint workspaces, Q37).
async fn run_task(
    client: Arc<HttpJson>,
    cfg: Arc<AgentRunnerConfig>,
    desired: Desired,
    killers: Killers,
    written: Written,
    shutdown: infra::Shutdown,
) {
    let missing_reported: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
    let backoff: Backoff = Arc::new(Mutex::new(BTreeMap::new()));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(RUN_PERIOD) => {}
        }
        let snapshot: Vec<AgentRecord> = desired.lock().await.values().cloned().collect();
        prune_backoff(&backoff, &snapshot).await;
        for record in snapshot {
            let view = &record.view;
            if view.phase == objs::PHASE_RUNNING && !killers.lock().await.contains_key(&view.key) {
                reset_orphan(&client, &written, &killers, view).await;
                continue;
            }
            if view.phase != objs::PHASE_PENDING {
                continue; // terminal or already handled
            }
            if killers.lock().await.contains_key(&view.key) {
                continue; // executing or reset in flight
            }
            // H2 (C14): a Pending Agent that keeps failing waits out its
            // backoff (500 ms doubling to 5 min) before the next attempt.
            let stamp = object_stamp(&record.raw);
            if backoff_active(backoff.lock().await.get(&view.key), &stamp, Instant::now()) {
                continue;
            }
            if cfg.runner_bin.is_none() {
                report_missing_runner(&client, &written, &missing_reported, view).await;
                continue;
            }
            // H3 (C14): the slot lives exactly as long as the guard below.
            let (slot, rx) = SlotGuard::acquire(killers.clone(), &view.key).await;
            tokio::spawn(execute_one(
                client.clone(),
                cfg.clone(),
                slot,
                backoff.clone(),
                written.clone(),
                record.clone(),
                rx,
            ));
        }
    }
}

/// H2 (C14): entries of objects that vanished from the desired map (deleted
/// or unscheduled) never accumulate.
async fn prune_backoff(backoff: &Backoff, snapshot: &[AgentRecord]) {
    let live: BTreeSet<&str> = snapshot.iter().map(|r| r.view.key.as_str()).collect();
    backoff
        .lock()
        .await
        .retain(|k, _| live.contains(k.as_str()));
}

/// Reset an orphan `Running` Agent to `Pending` (at-least-once, Q37). The
/// guard slot keeps this to one patch until the watch delivers `Pending`.
async fn reset_orphan(
    client: &HttpJson,
    written: &Written,
    killers: &Killers,
    view: &objs::AgentView,
) {
    let (tx, _rx) = watch::channel(true);
    killers.lock().await.insert(view.key.clone(), tx);
    let killers = killers.clone();
    let key = view.key.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        killers.lock().await.remove(&key);
    });
    let body = objs::status_patch(
        objs::PHASE_PENDING,
        &[fail_cond(
            "Restarted",
            "runner restarted while the Agent was Running; re-executing (at-least-once, Q37)",
        )],
    );
    patch_if_changed(client, written, view, &body).await;
}

/// `RunnerMissing` degradation: stay Pending, say why ONCE per object
/// instance (`name#uid`, see [`crate::agent_exec::dedupe_id`]) per boot.
async fn report_missing_runner(
    client: &HttpJson,
    written: &Written,
    reported: &Mutex<BTreeSet<String>>,
    view: &objs::AgentView,
) {
    if !reported.lock().await.insert(dedupe_id(view)) {
        return;
    }
    let body = objs::status_patch(
        objs::PHASE_PENDING,
        &[fail_cond(
            "RunnerMissing",
            "no opencoder runner binary staged; set INIT_PRO_AGENT_RUNNER_BIN or embed it (Q37)",
        )],
    );
    patch_if_changed(client, written, view, &body).await;
}

/// Execute one Agent to a terminal phase, then release its slot (the
/// `SlotGuard` does that on every exit path, H3/C14 -- including a panic
/// inside `run_attempt`).
async fn execute_one(
    client: Arc<HttpJson>,
    cfg: Arc<AgentRunnerConfig>,
    slot: SlotGuard,
    backoff: Backoff,
    written: Written,
    record: AgentRecord,
    mut keep_going: watch::Receiver<bool>,
) {
    let view = &record.view;
    let outcome = run_attempt(&client, &cfg, &written, &record, &mut keep_going).await;
    if let Some((phase, cond)) = outcome {
        patch_if_changed(&client, &written, view, &objs::status_patch(phase, &[cond])).await;
        // H2 (C14): a Pending outcome is "retry later" (SandboxRequired /
        // SandboxUnavailable) -> count it; anything terminal resets.
        let mut backoff = backoff.lock().await;
        if phase == objs::PHASE_PENDING {
            let next = backoff_after_failure(
                backoff.get(&view.key),
                &object_stamp(&record.raw),
                Instant::now(),
            );
            tracing::debug!(target: "init-pro", agent = %view.key, failures = next.failures,
                retry_in = ?next.next_retry.saturating_duration_since(Instant::now()),
                "agent stays Pending (C14 backoff)");
            backoff.insert(view.key.clone(), next);
        } else {
            backoff.remove(&view.key);
        }
    }
    drop(slot);
}

#[cfg(test)] // H2/H3 (C14) loop-guard cases split out (file size limit)
#[path = "agents_tests.rs"]
mod tests;
