//! `EngineBackend` (T4.12, Q49): [`crate::cri_backend::CriBackend`]
//! over per-container engine processes. Sandboxes are pause engines
//! (fresh netns); containers join `/proc/<pause_pid>/ns/net` and log to
//! the CRI text file directly (Q50). No image store/CNI until T4.13 —
//! only host-dir rootfs test pods run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cri_backend::{ContainerView, CriBackend, SandboxView};

use super::launch;
use super::pump::{self, Ctrl};
use super::registry::{Entry, Pending, Proc, Registry};
use super::spec;
use engine_client::EngineClient;
use runtime::{ContainerConfig, PodSandboxConfig};

/// SIGTERM grace before SIGKILL on stop (kube's 30s is generous for
/// test pods; T4.13 can make it spec-aware).
const STOP_GRACE: Duration = Duration::from_secs(10);
const KILL_GRACE: Duration = Duration::from_secs(5);
const STATE_POLL: Duration = Duration::from_millis(100);

pub struct EngineBackend {
    engine_dir: PathBuf,
    images_dir: PathBuf,
    sandbox_image: String,
    registry: Registry,
}

/// Engine errors that mean "gone already" — removes are idempotent.
fn already_gone(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("not found")
        || m.contains("no such file")
        || m.contains("not exist")
        || m.contains("already removed")
}

/// Distinctive id: time-ordered hex + a process-local counter.
fn new_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed) as u16;
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{prefix}{t:012x}{n:04x}")
}

impl EngineBackend {
    pub fn new(data_dir: &Path, sandbox_image: &str) -> std::io::Result<Self> {
        let engine_dir = launch::engine_dir(data_dir);
        std::fs::create_dir_all(&engine_dir)?;
        let images_dir = data_dir.join("images");
        Ok(Self {
            engine_dir,
            images_dir,
            sandbox_image: sandbox_image.to_string(),
            registry: Registry::default(),
        })
    }

    /// Restart hygiene (Q49): the agent's death killed the containers
    /// (client EOF), but engine processes linger idle. Ping every
    /// socket: live idlers get Shutdown; dead paths are unlinked.
    pub async fn recover(&self) {
        let mut cleaned = 0usize;
        if let Ok(rd) = std::fs::read_dir(&self.engine_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                    continue;
                }
                if pump::probe(&path).await.is_ok() {
                    if let Ok(client) = EngineClient::connect(&path).await {
                        let _ = client.shutdown().await;
                    }
                }
                let _ = std::fs::remove_file(&path);
                cleaned += 1;
            }
        }
        tracing::info!(
            target: "init-pro", cleaned,
            "builtin engine recovery: stopped stray engines (T4.12)"
        );
    }

    fn socket_for(&self, id: &str) -> PathBuf {
        self.engine_dir.join(format!("{id}.sock"))
    }

    /// Host-dir image resolution (T4.12 stopgap): an absolute directory
    /// or a name staged under `<data_dir>/images/`. Real store = T4.13.
    fn resolve_rootfs(&self, image: &str) -> Result<PathBuf, String> {
        let direct = Path::new(image);
        if direct.is_absolute() && direct.is_dir() {
            return Ok(direct.to_path_buf());
        }
        let staged = self.images_dir.join(image);
        if staged.is_dir() {
            return Ok(staged);
        }
        Err(format!(
            "no image store until T4.13: {image:?} is not a staged host-dir rootfs"
        ))
    }

    async fn wait_state(&self, id: &str, want: &str, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            match self.registry.state(id) {
                Some((state, _)) if state != want => {}
                _ => return true, // matched, or removed => settled
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(STATE_POLL).await;
        }
    }

    /// Stop a pause engine: Signal(15) -> engine Exited+Bye and exits
    /// (its own 10s grace); connect failures mean it is already gone.
    async fn stop_sandbox_engine(&self, id: &str) {
        let Some(socket) = self.registry.socket(id) else {
            return;
        };
        let signalled = match EngineClient::connect(&socket).await {
            Ok(client) => tokio::time::timeout(STOP_GRACE, client.signal(id, 15))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false),
            Err(_) => false,
        };
        if !signalled {
            if let Ok(client) = EngineClient::connect(&socket).await {
                let _ = client.shutdown().await;
            }
        }
        if let Some(mut child) = self.registry.take_child(id) {
            let _ = launch::reap(&mut child, STOP_GRACE);
        }
        let _ = std::fs::remove_file(&socket);
        self.registry.set_sandbox_notready(id);
    }

    fn sandbox_entry(cfg: &PodSandboxConfig) -> (String, String, String, BTreeMap<String, String>) {
        (
            cfg.metadata.name.clone(),
            cfg.metadata.namespace.clone(),
            cfg.metadata.uid.clone(),
            cfg.labels.clone(),
        )
    }
}

#[async_trait::async_trait]
impl CriBackend for EngineBackend {
    async fn list_image_tags(&self) -> Result<Vec<String>, String> {
        let mut tags = vec![self.sandbox_image.clone()];
        if let Ok(rd) = std::fs::read_dir(&self.images_dir) {
            for entry in rd.flatten() {
                if entry.path().is_dir() {
                    if let Ok(name) = entry.file_name().into_string() {
                        tags.push(name);
                    }
                }
            }
        }
        Ok(tags)
    }

    async fn pull_image(&self, image: &str) -> Result<(), String> {
        if image == self.sandbox_image {
            // The pause engine parks before exec — no rootfs needed.
            return Ok(());
        }
        if self.resolve_rootfs(image).is_ok() {
            return Ok(());
        }
        Err(format!("no image store until T4.13: cannot pull {image:?}"))
    }

    async fn list_sandboxes(&self) -> Result<Vec<SandboxView>, String> {
        Ok(self.registry.sandbox_views())
    }

    async fn list_containers(&self) -> Result<Vec<ContainerView>, String> {
        Ok(self.registry.container_views())
    }

    async fn run_pod_sandbox(&self, cfg: &PodSandboxConfig) -> Result<String, String> {
        let id = new_id("sb-");
        let socket = self.socket_for(&id);
        let child =
            launch::spawn_engine(&socket, true).map_err(|e| format!("spawn pause engine: {e}"))?;
        launch::wait_socket(&socket)
            .await
            .map_err(|e| format!("pause engine never came up: {e}"))?;
        let pause_pid = EngineClient::connect(&socket)
            .await
            .map_err(|e| format!("pause engine ping: {e}"))?
            .ping_pid()
            .await
            .map_err(|e| format!("pause engine ping: {e}"))?
            .ok_or_else(|| "pause engine answered without a pid".to_string())?;
        let (name, namespace, uid, labels) = Self::sandbox_entry(cfg);
        self.registry.insert(Entry {
            id: id.clone(),
            socket,
            proc: Proc::Sandbox(Some(child)),
            state: "SANDBOX_READY".into(),
            exit_code: None,
            pid: Some(pause_pid),
            netns: Some(format!("/proc/{pause_pid}/ns/net")),
            name,
            namespace,
            uid,
            attempt: cfg.metadata.attempt,
            labels,
            pending: None,
            ctrl: None,
        });
        Ok(id)
    }

    async fn create_container(
        &self,
        pod_sandbox_id: &str,
        ccfg: &ContainerConfig,
        pcfg: &PodSandboxConfig,
    ) -> Result<String, String> {
        let id = new_id("c-");
        self.registry.insert(Entry {
            id: id.clone(),
            socket: self.socket_for(&id),
            proc: Proc::Container { engine_pid: 0 },
            state: "CONTAINER_CREATED".into(),
            exit_code: None,
            pid: None,
            netns: None,
            name: ccfg.metadata.name.clone(),
            namespace: pcfg.metadata.namespace.clone(),
            uid: pcfg.metadata.uid.clone(),
            attempt: ccfg.metadata.attempt,
            labels: ccfg.labels.clone(),
            pending: Some(Pending {
                ccfg: ccfg.clone(),
                pcfg: pcfg.clone(),
                sandbox_id: pod_sandbox_id.to_string(),
            }),
            ctrl: None,
        });
        Ok(id)
    }

    async fn start_container(&self, id: &str) -> Result<(), String> {
        let Some(socket) = self.registry.socket(id) else {
            return Err(format!("start: unknown container {id}"));
        };
        let pending = self
            .registry
            .pending(id)
            .ok_or_else(|| format!("start: container {id} has nothing pending"))?;
        let sandbox_netns = self
            .registry
            .netns_of(&pending.sandbox_id)
            .ok_or_else(|| format!("sandbox {} is not ready", pending.sandbox_id))?;
        let rootfs = self.resolve_rootfs(&pending.ccfg.image.image)?;
        let es = spec::container_spec(id, &rootfs, &pending, sandbox_netns)?;
        let child =
            launch::spawn_engine(&socket, false).map_err(|e| format!("spawn engine: {e}"))?;
        launch::wait_socket(&socket)
            .await
            .map_err(|e| format!("engine never came up: {e}"))?;
        let client = EngineClient::connect(&socket)
            .await
            .map_err(|e| format!("connect engine: {e}"))?;
        let stream = client
            .run(es)
            .await
            .map_err(|e| format!("engine refused run: {e}"))?;
        let ctrl = pump::spawn_pump(self.registry.clone(), id.to_string(), socket, stream, child);
        self.registry.set_ctrl(id, ctrl);
        Ok(())
    }

    async fn stop_container(&self, id: &str) -> Result<(), String> {
        match self.registry.state(id) {
            None => return Ok(()),
            Some((state, _)) if state != "CONTAINER_RUNNING" => return Ok(()),
            _ => {}
        }
        if let Some(ctrl) = self.registry.ctrl(id) {
            let _ = ctrl.send(Ctrl::Signal(15)).await;
            if self.wait_state(id, "CONTAINER_EXITED", STOP_GRACE).await {
                return Ok(());
            }
            let _ = ctrl.send(Ctrl::Signal(9)).await;
            if self.wait_state(id, "CONTAINER_EXITED", KILL_GRACE).await {
                return Ok(());
            }
            let _ = ctrl.send(Ctrl::Shutdown).await;
            self.wait_state(id, "CONTAINER_EXITED", KILL_GRACE).await;
        }
        Ok(())
    }

    async fn remove_container(&self, id: &str) -> Result<(), String> {
        if !self.registry.contains(id) {
            return Ok(()); // idempotent
        }
        if let Some(ctrl) = self.registry.ctrl(id) {
            let _ = ctrl.send(Ctrl::Shutdown).await;
            self.wait_state(id, "CONTAINER_EXITED", KILL_GRACE).await;
        }
        self.registry.remove(id);
        Ok(())
    }

    async fn stop_pod_sandbox(&self, id: &str) -> Result<(), String> {
        if self.registry.contains(id) {
            self.stop_sandbox_engine(id).await;
        }
        Ok(())
    }

    async fn remove_pod_sandbox(&self, id: &str) -> Result<(), String> {
        self.stop_pod_sandbox(id).await?;
        self.registry.remove(id);
        Ok(())
    }

    async fn exec_container(
        &self,
        container_id: &str,
        cmd: &[String],
        timeout: Duration,
    ) -> Result<(i32, String), String> {
        match super::exec::exec(&self.registry, container_id, cmd.to_vec(), timeout).await {
            Ok(v) => Ok(v),
            Err(e) if already_gone(&e) => Ok((255, e)),
            Err(e) => Err(e),
        }
    }
}
