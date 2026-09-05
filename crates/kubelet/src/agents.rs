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

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{watch, Mutex};

use crate::agent_exec::{fail_cond, patch_if_changed, run_attempt, Written};
use crate::agent_objects as objs;
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
    /// Fallback anyhome NFS address when an Agent carries no workspace
    /// annotation (`--anyhome-addr`; the single-node default needs no flag).
    pub anyhome_addr: Option<String>,
    /// One attempt budget (`INIT_PRO_AGENT_EXEC_TIMEOUT_SECS`).
    pub exec_timeout: Duration,
}

impl AgentRunnerConfig {
    pub fn new(server_url: String, node_name: String, data_dir: PathBuf) -> Self {
        Self {
            server_url,
            node_name,
            data_dir,
            runner_bin: None,
            anyhome_addr: None,
            exec_timeout: exec_timeout_from_env().unwrap_or(DEFAULT_EXEC_TIMEOUT),
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

/// Spawn the watch + run loops (same shape as [`crate::runner::spawn`]).
pub fn spawn_agent_runner(
    cfg: AgentRunnerConfig,
    shutdown: infra::Shutdown,
) -> Vec<tokio::task::JoinHandle<()>> {
    let client = match HttpJson::parse_url(&cfg.server_url) {
        Ok(c) => Arc::new(c),
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
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(RUN_PERIOD) => {}
        }
        let snapshot: Vec<AgentRecord> = desired.lock().await.values().cloned().collect();
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
            if cfg.runner_bin.is_none() {
                report_missing_runner(&client, &written, &missing_reported, view).await;
                continue;
            }
            let (tx, rx) = watch::channel(true);
            killers.lock().await.insert(view.key.clone(), tx);
            tokio::spawn(execute_one(
                client.clone(),
                cfg.clone(),
                killers.clone(),
                written.clone(),
                record.clone(),
                rx,
            ));
        }
    }
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

/// `RunnerMissing` degradation: stay Pending, say why ONCE per key per boot.
async fn report_missing_runner(
    client: &HttpJson,
    written: &Written,
    reported: &Mutex<BTreeSet<String>>,
    view: &objs::AgentView,
) {
    if !reported.lock().await.insert(view.key.clone()) {
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

/// Execute one Agent to a terminal phase, then release its slot.
async fn execute_one(
    client: Arc<HttpJson>,
    cfg: Arc<AgentRunnerConfig>,
    killers: Killers,
    written: Written,
    record: AgentRecord,
    mut keep_going: watch::Receiver<bool>,
) {
    let outcome = run_attempt(&client, &cfg, &written, &record, &mut keep_going).await;
    if let Some((phase, cond)) = outcome {
        patch_if_changed(
            &client,
            &written,
            &record.view,
            &objs::status_patch(phase, &[cond]),
        )
        .await;
    }
    killers.lock().await.remove(&record.view.key);
}
