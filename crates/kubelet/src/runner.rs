//! The kubelet loops (TODO **T4.2**): three tasks spawned by [`spawn`] —
//! a pod watch (LIST + watch stream -> desired map), the sync cycle
//! (snapshot -> [`crate::sync::plan`] -> execute -> `/status` writes), and
//! node registration + `kube-node-lease` heartbeat. All errors are logged
//! and retried; nothing panics. Tracing target is `"init-pro"`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::cri_backend::CriBackend;
use crate::exec::{execute, snapshot};
use crate::http::HttpJson;
use crate::node::{node_task, now_str};
use crate::objects::{self, PodView};
use crate::status::{build_pod_status, merge_pod_for_status, status_semantically_eq};
use crate::sync::plan;

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
        }
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

/// Spawn the three kubelet tasks; join handles drain on shutdown.
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
            cri,
            cfg.clone(),
            desired,
            shutdown.clone(),
        )),
        tokio::spawn(node_task(client, cfg, shutdown)),
    ]
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
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        sync_once(&client, cri.as_ref(), &cfg, &desired, &mut last_status).await;
    }
}

async fn sync_once(
    client: &HttpJson,
    cri: &dyn CriBackend,
    cfg: &KubeletConfig,
    desired: &Desired,
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
    let actions = plan(&wanted, &snap, &cfg.sandbox_image, &cfg.log_root());
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
    // Status pass: PUT only when the semantic status actually changed.
    let now = now_str();
    for (key, view) in &wanted {
        if view.deleted {
            continue;
        }
        let status = build_pod_status(view, &snap, &now);
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
}
