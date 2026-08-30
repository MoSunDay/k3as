//! The kubelet loops (TODO **T4.2**): four tasks spawned by [`spawn`] —
//! a pod watch (LIST + watch stream -> desired map), the sync cycle
//! (snapshot -> [`crate::sync::plan`] -> execute -> `/status` writes), node
//! registration + `kube-node-lease` heartbeat, and since Scope C (Q31) the
//! local container log/exec HTTP listener ([`crate::server::serve`]). All
//! errors are logged and retried; nothing panics. Tracing target is
//! `"init-pro"`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::cri_backend::CriBackend;
use crate::exec::{execute, snapshot};
use crate::http::HttpJson;
use crate::node::{node_task, now_str};
use crate::objects::{self, PodView};
use crate::probes::{run_http_probe, PodProbeState, ProbeKind};
use crate::status::{build_pod_status, merge_pod_for_status, status_semantically_eq};
use crate::sync::{plan, Snapshot};
use crate::volumes;

/// Configuration for [`spawn`]. All knobs are public so callers (the
/// `agent` CLI wiring) can override; [`KubeletConfig::new`] gives k3s-like
/// defaults (2s sync, 10s heartbeat, env-driven sandbox image).
#[derive(Debug, Clone)]
pub struct KubeletConfig {
    pub server_url: String,
    pub node_name: String,
    pub data_dir: PathBuf,
    pub sandbox_image: String,
    pub sync_period: Duration,
    pub heartbeat_period: Duration,
    /// IP the kubelet's local log/exec listener binds AND that it advertises
    /// on the Node object (`status.addresses[InternalIP]`) so the apiserver
    /// relay can reach it (T4.2 Scope C, Q31). Default `127.0.0.1`.
    pub node_ip: String,
    /// Port of the same listener (upstream kubelet convention: 10250);
    /// published via the Node annotation `init-pro.io/kubelet-port`.
    pub kubelet_port: u16,
}

impl KubeletConfig {
    /// Defaults: sandbox image from `INIT_PRO_SANDBOX_IMAGE` (else the
    /// pinned [`runtime::DEFAULT_SANDBOX_IMAGE`]), 2s sync, 10s heartbeat.
    pub fn new(
        server_url: impl Into<String>,
        node_name: impl Into<String>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            server_url: server_url.into(),
            node_name: node_name.into(),
            data_dir: data_dir.into(),
            sandbox_image: runtime::config::sandbox_image(),
            sync_period: Duration::from_secs(2),
            heartbeat_period: Duration::from_secs(10),
            node_ip: "127.0.0.1".to_string(),
            kubelet_port: 10250,
        }
    }

    /// Override the advertised/bind node IP for the local log+exec listener.
    pub fn with_node_ip(mut self, ip: impl Into<String>) -> Self {
        self.node_ip = ip.into();
        self
    }

    /// Override the local log+exec listener port (default 10250).
    pub fn with_kubelet_port(mut self, port: u16) -> Self {
        self.kubelet_port = port;
        self
    }

    /// Per-pod log root handed to the CRI sandbox configs.
    pub fn log_root(&self) -> PathBuf {
        self.data_dir.join("pods")
    }
}

/// Node name default: `$HOSTNAME`, else `hostname` output, else a constant.
pub fn default_node_name() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        let h = h.trim().to_string();
        if !h.is_empty() {
            return h;
        }
    }
    if let Ok(out) = std::process::Command::new("hostname").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "init-pro-node".to_string()
}

/// Spawn the kubelet tasks; join handles drain on shutdown.
///
/// Four loops since T4.2 Scope C: pod watch, sync cycle, node/lease
/// heartbeat, and the local log/exec HTTP listener ([`crate::server::serve`],
/// Q31) — the apiserver relay dials it on `cfg.node_ip:cfg.kubelet_port`.
pub fn spawn(
    cfg: KubeletConfig,
    cri: Arc<dyn CriBackend>,
    shutdown: infra::Shutdown,
) -> Vec<JoinHandle<()>> {
    let client = match HttpJson::parse_url(&cfg.server_url) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!(target: "init-pro", error = %e, "kubelet: bad server URL; not starting");
            return Vec::new();
        }
    };
    let desired: Arc<Mutex<BTreeMap<String, PodView>>> = Arc::new(Mutex::new(BTreeMap::new()));
    tracing::info!(
        target: "init-pro",
        node = %cfg.node_name, server = %cfg.server_url,
        local = %format!("{}:{}", cfg.node_ip, cfg.kubelet_port),
        "kubelet loops starting (T4.2)"
    );
    vec![
        tokio::spawn(watch_task(
            client.clone(),
            cfg.node_name.clone(),
            desired.clone(),
            shutdown.clone(),
        )),
        tokio::spawn(sync_task(
            client.clone(),
            cri.clone(),
            cfg.clone(),
            desired,
            shutdown.clone(),
        )),
        tokio::spawn(node_task(client, cfg.clone(), shutdown.clone())),
        // Task 4 (T4.2 Scope C / Q31): plain-HTTP container logs + exec for
        // the apiserver relay. Same `CriCtl`-backed seam as the sync loop;
        // a bind failure must not take the whole kubelet down, so the error
        // is logged inside [`server_task`] instead of failing `spawn`.
        tokio::spawn(server_task(cfg, cri, shutdown)),
    ]
}

/// Adapter around [`crate::server::serve`] so the join set stays
/// `JoinHandle<()>`: bind failures (bad node_ip / port already taken) are
/// logged and end this one task only.
async fn server_task(cfg: KubeletConfig, backend: Arc<dyn CriBackend>, shutdown: infra::Shutdown) {
    if let Err(e) = crate::server::serve(cfg, backend, shutdown).await {
        tracing::warn!(target: "init-pro", error = %e, "kubelet: local log/exec API exited");
    }
}

type Desired = Arc<Mutex<BTreeMap<String, PodView>>>;

/// Task 1: LIST + watch `/api/v1/pods`, keeping the desired map current.
async fn watch_task(
    client: Arc<HttpJson>,
    node_name: String,
    desired: Desired,
    shutdown: infra::Shutdown,
) {
    loop {
        if shutdown.is_triggered() {
            return;
        }
        if let Err(e) = watch_cycle(&client, &node_name, &desired, &shutdown).await {
            if shutdown.is_triggered() {
                return;
            }
            tracing::debug!(target: "init-pro", error = %e, "kubelet: pod watch cycle ended; re-LIST in 500ms");
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
    shutdown: &infra::Shutdown,
) -> Result<(), String> {
    // Relist: replace the desired map wholesale, then follow live events.
    let (code, list) = client
        .get_json("/api/v1/pods")
        .await
        .map_err(|e| e.to_string())?;
    if code != 200 {
        return Err(format!("LIST pods -> HTTP {code}"));
    }
    let mut map = BTreeMap::new();
    for item in list
        .pointer("/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if objects::pod_node(item).as_deref() == Some(node_name) {
            if let Some(v) = objects::pod_view(item) {
                // Soft-deleted pods stay desired (deleted=true) so their
                // CRI state gets torn down; hard-deleted ones are absent.
                map.insert(v.key.clone(), v);
            }
        }
    }
    *desired.lock().await = map;
    let rv = list
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .unwrap_or("0");
    let mut conn = client
        .watch(&format!("/api/v1/pods?watch=1&resourceVersion={rv}"))
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
        let typ = ev
            .pointer("/type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let obj = ev.pointer("/object").cloned().unwrap_or(Value::Null);
        apply_event(&typ, &obj, node_name, &mut *desired.lock().await);
    }
}

/// Fold one watch event into the desired map (pure map mutation). A hard
/// DELETE leaves a `deleted=true` tombstone so the sync cycle can tear down
/// CRI state before pruning it; soft deletes arrive via `deletionTimestamp`.
fn apply_event(typ: &str, obj: &Value, node_name: &str, map: &mut BTreeMap<String, PodView>) {
    let Some(key) = objects::pod_key(obj) else {
        return;
    };
    let ours = objects::pod_node(obj).as_deref() == Some(node_name);
    if typ == "DELETED" {
        if !ours {
            map.remove(&key);
            return;
        }
        if let Some(mut view) = objects::pod_view(obj) {
            view.deleted = true;
            map.insert(key, view);
        }
        return;
    }
    if typ != "ADDED" && typ != "MODIFIED" {
        return;
    }
    let Some(view) = objects::pod_view(obj) else {
        return;
    };
    if ours {
        map.insert(key, view);
    } else {
        map.remove(&key);
    }
}

/// Task 2: every sync period — snapshot, plan, execute, report statuses.
async fn sync_task(
    client: Arc<HttpJson>,
    cri: Arc<dyn CriBackend>,
    cfg: KubeletConfig,
    desired: Desired,
    shutdown: infra::Shutdown,
) {
    let mut tick = tokio::time::interval(cfg.sync_period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_status: BTreeMap<String, Value> = BTreeMap::new();
    let mut probes = PodProbeState::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                // T4.1 follow-up (Sprint 28): tear every sandbox down before
                // this task exits, so the agent can stop containerd without
                // orphaning shims. Bounded; the supervisor fires only after
                // the caller joins THIS task, so the order is structural.
                let n = crate::sync::drain_sandboxes(
                    cri.as_ref(),
                    crate::sync::SANDBOX_DRAIN_BUDGET,
                )
                .await;
                tracing::info!(
                    target: "init-pro",
                    node = %cfg.node_name,
                    sandboxes = n,
                    "kubelet drained (sandbox teardown done)"
                );
                return;
            }
            _ = tick.tick() => {}
        }
        sync_once(
            &client,
            cri.as_ref(),
            &cfg,
            &desired,
            &mut probes,
            &mut last_status,
        )
        .await;
    }
}

async fn sync_once(
    client: &HttpJson,
    cri: &dyn CriBackend,
    cfg: &KubeletConfig,
    desired: &Desired,
    probes: &mut PodProbeState,
    last_status: &mut BTreeMap<String, Value>,
) {
    let snap = match snapshot(cri).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "kubelet: CRI snapshot failed");
            return;
        }
    };
    let wanted = desired.lock().await.clone();
    // Probe pass (Sprint 20 S1, T4.2 Scope B): drive due HTTP probes for
    // RUNNING containers inside READY sandboxes that have an IP. The
    // outcomes feed both the plan (liveness restarts) and the status
    // writer (readiness gate). Sandboxes without an IP are skipped and
    // their probe status stays Unknown.
    let now = unix_now();
    probe_pass(&wanted, &snap, probes, now).await;
    let keep: BTreeSet<String> = wanted.keys().cloned().collect();
    probes.prune(&keep);
    // Volume pass (Sprint 20 S2, T4.2 Scope B): materialize emptyDir dirs
    // and fingerprint-refreshed configMap/secret files BEFORE plan so the
    // CRI create (which mounts these host paths) sees them. Failures are
    // logged; the cycle retries.
    if let Err(e) = volumes::ensure_volumes(client, &wanted, &cfg.log_root()).await {
        tracing::warn!(target: "init-pro", error = %e, "kubelet: volume pass failed");
    }
    let actions = plan(&wanted, &snap, &cfg.sandbox_image, &cfg.log_root(), probes);
    // Deleted pods needing no further teardown drop out of the desired map.
    {
        let mut map = desired.lock().await;
        map.retain(|k, v| !v.deleted || actions.iter().any(|a| a.pod_key() == Some(k.as_str())));
    }
    for (action, res) in execute(cri, actions).await {
        if let Err(e) = res {
            tracing::warn!(target: "init-pro", action = ?action, error = %e, "kubelet: CRI action failed");
        }
    }
    // Volume cleanup (S2): pods whose sandbox is already gone release
    // their volume root. The PRE-execute snap is used deliberately: a pod
    // torn down this cycle still shows its sandbox there, so cleanup lands
    // one cycle later — race-free. Pods that never existed simply have no
    // dir (cleanup tolerates NotFound).
    for (key, uid) in cleanup_candidates(&wanted, &snap) {
        if let Err(e) = volumes::cleanup_volumes(&cfg.log_root(), &uid).await {
            tracing::warn!(target: "init-pro", pod = %key, error = %e, "kubelet: volume cleanup failed");
        }
    }
    // Status pass: PUT only when the semantic status actually changed.
    let now = now_str();
    for (key, view) in &wanted {
        if view.deleted {
            continue;
        }
        let status = build_pod_status(view, &snap, &now, probes);
        if matches!(last_status.get(key), Some(prev) if status_semantically_eq(prev, &status)) {
            continue;
        }
        let body = merge_pod_for_status(&objects::pod_stub(view), &status);
        let path = format!(
            "/api/v1/namespaces/{ns}/pods/{name}/status",
            ns = view.namespace,
            name = view.name
        );
        match client.put_json(&path, &body).await {
            Ok((200, _)) => {
                last_status.insert(key.clone(), status);
            }
            Ok((code, resp)) => {
                tracing::debug!(target: "init-pro", code = %code, %resp, "kubelet: status PUT rejected");
            }
            Err(e) => tracing::debug!(target: "init-pro", error = %e, "kubelet: status PUT failed"),
        }
    }
    last_status.retain(|k, _| wanted.contains_key(k));
}

/// Drive due HTTP probes for RUNNING containers of READY sandboxes that
/// have an IP (Sprint 20 S1, T4.2 Scope B). Trackers arm from the first
/// RUNNING observation — CRI reports no container start time in S1, so
/// that observation time is the start proxy (initialDelaySeconds counts
/// from it). Pods without an IP are skipped entirely; their probe status
/// stays Unknown until a probe actually runs.
async fn probe_pass(
    wanted: &BTreeMap<String, PodView>,
    snap: &Snapshot,
    probes: &mut PodProbeState,
    now: u64,
) {
    for view in wanted.values() {
        if view.deleted {
            continue;
        }
        let Some(sb) = snap
            .sandboxes
            .iter()
            .find(|s| s.namespace == view.namespace && s.name == view.name)
        else {
            continue;
        };
        if sb.state != "SANDBOX_READY" {
            continue;
        }
        let Some(host) = sb.ip.clone() else {
            continue;
        };
        for spec in &view.containers {
            let kinds = [
                (ProbeKind::Startup, spec.startup_probe.as_ref()),
                (ProbeKind::Readiness, spec.readiness_probe.as_ref()),
                (ProbeKind::Liveness, spec.liveness_probe.as_ref()),
            ];
            if !kinds.iter().any(|(_, p)| p.is_some()) {
                continue;
            }
            // The live incarnation: highest-attempt RUNNING container.
            let Some(cv) = snap
                .containers
                .iter()
                .filter(|c| {
                    c.sandbox_id == sb.id && c.name == spec.name && c.state == "CONTAINER_RUNNING"
                })
                .max_by_key(|c| c.attempt)
            else {
                continue;
            };
            probes.note_incarnation(&view.key, &spec.name, &cv.id, cv.attempt);
            let state = probes.entry(&view.key, &spec.name);
            for (kind, pspec) in kinds {
                let Some(pspec) = pspec else { continue };
                let tracker = state.tracker(kind);
                tracker.arm(now, pspec);
                if tracker.due(now) {
                    let timeout = Duration::from_secs(u64::from(pspec.timeout_seconds));
                    let res =
                        run_http_probe(&pspec.http_get, &host, pspec.http_get.port, timeout).await;
                    match res {
                        Ok(()) => tracker.observe(true, now, pspec),
                        Err(e) => {
                            tracing::debug!(
                                target: "init-pro",
                                pod = %view.key,
                                container = %spec.name,
                                error = %e,
                                "kubelet: probe failed"
                            );
                            tracker.observe(false, now, pspec);
                        }
                    }
                }
            }
        }
    }
}

/// Pods whose volumes may be released (Sprint 20 S2): deleted pods still
/// in the desired map (tombstones carry the uid) plus sandboxed pods
/// absent from it (hard-deleted or rescheduled away — the uid comes from
/// the sandbox). Sorted + deduped so cleanup runs once per pod.
fn cleanup_candidates(
    wanted: &BTreeMap<String, PodView>,
    snap: &Snapshot,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (key, view) in wanted {
        if view.deleted {
            out.push((key.clone(), view.uid.clone()));
        }
    }
    for sb in &snap.sandboxes {
        if sb.name.is_empty() || sb.uid.is_empty() {
            continue; // garbage sandbox without identity: nothing to clean
        }
        let key = format!("{}/{}", sb.namespace, sb.name);
        if !wanted.contains_key(&key) {
            out.push((key, sb.uid.clone()));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Unix seconds now; 0 on clock errors (degrades to "immediately due",
/// which is safe: probe scheduling only ever fires early).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        std::env::remove_var("INIT_PRO_SANDBOX_IMAGE");
        let cfg = KubeletConfig::new("http://127.0.0.1:6443", "n1", "/tmp/dd");
        assert_eq!(cfg.sandbox_image, runtime::DEFAULT_SANDBOX_IMAGE);
        assert_eq!(cfg.sync_period, Duration::from_secs(2));
        assert_eq!(cfg.heartbeat_period, Duration::from_secs(10));
        assert_eq!(cfg.log_root(), PathBuf::from("/tmp/dd/pods"));
    }

    #[test]
    fn apply_event_upserts_tombstones_and_filters() {
        let mut map = BTreeMap::new();
        let pod = serde_json::json!({
            "metadata": {"name": "p", "namespace": "default", "uid": "u"},
            "spec": {"nodeName": "n1"},
        });
        apply_event("ADDED", &pod, "n1", &mut map);
        assert_eq!(map.len(), 1);
        // Rescheduled away -> removed.
        let mut away = pod.clone();
        away["spec"]["nodeName"] = serde_json::json!("other");
        apply_event("MODIFIED", &away, "n1", &mut map);
        assert!(map.is_empty());
        // Hard delete leaves a deleted tombstone for teardown.
        apply_event("ADDED", &pod, "n1", &mut map);
        apply_event("DELETED", &pod, "n1", &mut map);
        assert_eq!(map.len(), 1);
        assert!(map["default/p"].deleted);
        // Soft delete (deletionTimestamp) stays desired with deleted=true.
        let mut soft = pod.clone();
        soft["metadata"]["deletionTimestamp"] = serde_json::json!("2026-08-16T00:00:00Z");
        apply_event("MODIFIED", &soft, "n1", &mut map);
        assert_eq!(map.len(), 1);
        assert!(map["default/p"].deleted);
        // Unknown event types are ignored.
        apply_event("BOOKMARK", &pod, "n1", &mut map);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn default_node_name_is_never_empty() {
        assert!(!default_node_name().is_empty());
    }

    #[test]
    fn cleanup_candidates_collect_deleted_and_orphaned_pods() {
        use crate::cri_backend::SandboxView;
        let mut wanted = BTreeMap::new();
        let alive = serde_json::json!({
            "metadata": {"name": "a", "namespace": "d", "uid": "u-a"},
            "spec": {"nodeName": "n1"},
        });
        let mut dying = alive.clone();
        dying["metadata"]["name"] = serde_json::json!("gone");
        dying["metadata"]["uid"] = serde_json::json!("u-gone");
        dying["metadata"]["deletionTimestamp"] = serde_json::json!("2026-08-23T00:00:00Z");
        apply_event("ADDED", &alive, "n1", &mut wanted);
        apply_event("ADDED", &dying, "n1", &mut wanted);
        let sb = |name: &str, uid: &str| SandboxView {
            id: format!("sb-{name}"),
            state: "SANDBOX_READY".into(),
            labels: Default::default(),
            name: name.into(),
            namespace: "d".into(),
            uid: uid.into(),
            ip: None,
        };
        let snap = Snapshot {
            sandboxes: vec![sb("a", "u-a"), sb("orphan", "u-o"), sb("", "")],
            ..Default::default()
        };
        let cands = cleanup_candidates(&wanted, &snap);
        assert_eq!(
            cands,
            vec![
                ("d/gone".to_string(), "u-gone".to_string()),
                ("d/orphan".to_string(), "u-o".to_string()),
            ],
            "deleted tombstone + sandbox absent from wanted; alive and garbage skipped"
        );
    }
}
