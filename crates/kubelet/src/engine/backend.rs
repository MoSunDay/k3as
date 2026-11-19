//! `EngineBackend` (T4.12, Q49): [`crate::cri_backend::CriBackend`]
//! over per-container engine processes. Sandboxes are pause engines
//! sharing the host netns (Q52): no CLONE_NEWNET for the pause child;
//! containers join `/proc/<pause_pid>/ns/net` (= host netns) and log to
//! the CRI text file directly (Q50). Since T4.13 the image store is REAL:
//! registry-v2 pulls land in the content-addressed CAS
//! (`engine/images`, Q53/Q54), layers are unpacked ONCE into shared
//! per-layer dirs, and each container gets a per-container overlayfs
//! view (or the whiteout-aware fused-copy fallback when the kernel path
//! is unavailable). CNI stays never-by-design for builtin pods (Q52:
//! host netns).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cri_backend::{ContainerView, CriBackend, SandboxView};

use super::image::pull::Puller;
use super::image::{ConfigBody, ImageStore, PulledImage};
use super::launch;
use super::pump::{self, Ctrl};
use super::registry::{Entry, Pending, Proc, Registry};
use super::spec;
use engine_client::EngineClient;
use runtime::{ContainerConfig, PodSandboxConfig};

/// Default SIGTERM grace before SIGKILL on stop (kube's 30s is generous
/// for test pods). T4.13 M4 (Q53): a pod annotation
/// `init-pro.io/stop-grace-seconds` overrides it per container (clamped
/// to 1..=600s; see [`EngineBackend::stop_grace`]).
const STOP_GRACE: Duration = Duration::from_secs(10);
const KILL_GRACE: Duration = Duration::from_secs(5);
const STATE_POLL: Duration = Duration::from_millis(100);

pub struct EngineBackend {
    engine_dir: PathBuf,
    images_dir: PathBuf,
    sandbox_image: String,
    node_ip: String,
    /// T4.13 M2: the content-addressed image store under
    /// `<data_dir>/engine/images` (Q53/Q54) + the pull client bound to the
    /// configured registry.
    store: ImageStore,
    puller: Puller,
    registry: Registry,
    /// T4.13 M3 (Q53): overlayfs availability probed once at startup.
    /// True = per-container overlayfs views over shared unpacked layers;
    /// false keeps the whiteout-aware fused-copy fallback.
    overlay_ok: bool,
}

/// Engine errors that mean "gone already" — removes are idempotent.
fn already_gone(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("not found")
        || m.contains("no such file")
        || m.contains("not exist")
        || m.contains("already removed")
}

/// Where a container's `/` comes from (T4.13 M3, Q53): a plain directory
/// (host dir, fused merge, or a T4.12 staged name) or a per-container
/// overlayfs view over shared unpacked layers.
enum ResolvedRootfs {
    Dir(PathBuf),
    Overlay {
        mountpoint: PathBuf,
        lower: Vec<PathBuf>,
        upper: PathBuf,
        work: PathBuf,
    },
}

/// Named-image-user resolution (T4.13 M4, Q53): scan the candidate
/// rootfs/layer dirs TOPMOST first (the last layer that ships
/// `/etc/passwd` wins, matching unpack order) and look the name up in
/// it. Reads only; a missing passwd anywhere means `None`.
fn named_user_lookup(dirs: &[PathBuf]) -> impl Fn(&str) -> Option<(u32, u32)> + use<'_> {
    move |name: &str| {
        dirs.iter()
            .rev()
            .filter_map(|dir| std::fs::read_to_string(dir.join("etc/passwd")).ok())
            .find_map(|content| spec::parse_passwd_uids(&content).get(name).copied())
    }
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
    /// `registry_addr` (T4.13 M2, Q54): the default registry image
    /// references without their own host pull from.
    pub fn new(
        data_dir: &Path,
        sandbox_image: &str,
        node_ip: &str,
        registry_addr: &str,
    ) -> std::io::Result<Self> {
        let engine_dir = launch::engine_dir(data_dir);
        std::fs::create_dir_all(&engine_dir)?;
        let images_dir = data_dir.join("images");
        let images_root = data_dir.join("engine").join("images");
        let store = ImageStore::open(&images_root)?;
        // T4.13 M3 (Q53): one root-gated probe decides the rootfs path —
        // per-container overlayfs views or the fused-copy fallback.
        let overlay_ok = super::image::probe::overlay_supported(&images_root.join(".probe"));
        tracing::info!(
            target: "init-pro",
            overlay_ok,
            "builtin image rootfs: {} (Q53)",
            if overlay_ok {
                "per-container overlayfs views"
            } else {
                "fused-copy fallback"
            }
        );
        let puller = Puller::new(registry_addr, store.clone());
        Ok(Self {
            engine_dir,
            images_dir,
            sandbox_image: sandbox_image.to_string(),
            node_ip: node_ip.to_string(),
            store,
            puller,
            registry: Registry::default(),
            overlay_ok,
        })
    }

    /// T4.13 M5: soft quota cap (bytes) on the builtin image store —
    /// checked BEFORE a pull downloads blobs/layers; 0 = unlimited (the
    /// default, matching the CLI flag's default-off posture).
    pub fn with_image_quota(mut self, bytes: u64) -> Self {
        self.puller = self.puller.with_quota(bytes);
        self
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
        // T4.13 M3 (Q53): the engines that owned the overlay views are
        // gone; their upper/work scratch is dead weight. The shared layer
        // dirs stay (they are the point of the unpack-once cache).
        self.store.clear_views();
        // T4.13 M5 (Q53): startup GC trigger — sweep CAS blobs, per-layer
        // dirs and fused merges no ref file still owns (the ref table is
        // the unit of ownership; the API delete face drops refs). Safe
        // ONLY because engines die with the agent (Q48/Q49 PDEATHSIG
        // chain): no live overlay view can reference a swept layer. If
        // engines ever outlive the agent, this must first confirm no
        // live views (review follow-up note, Sprint 43).
        match self.store.gc() {
            Ok((blobs, merged, layers)) => tracing::info!(
                target: "init-pro", blobs, layers, merged,
                "builtin image GC: {blobs} blob(s), {layers} layer dir(s), {merged} merged dir(s) removed"
            ),
            Err(e) => tracing::warn!(
                target: "init-pro", error = %e,
                "builtin image GC failed at startup (continuing)"
            ),
        }
        tracing::info!(
            target: "init-pro", cleaned,
            "builtin engine recovery: stopped stray engines (T4.12)"
        );
    }

    fn socket_for(&self, id: &str) -> PathBuf {
        self.engine_dir.join(format!("{id}.sock"))
    }

    /// Image rootfs resolution (T4.13): (1) an absolute host directory,
    /// (2) a pulled CAS reference (by normalized ref, or by digest for
    /// `repo@sha256:` spellings) whose shared layers exist — an overlayfs
    /// view when the kernel path is available (Q53), else the cached
    /// whiteout-aware fused merge, (3) a name staged under
    /// `<data_dir>/images/`.
    fn resolve(&self, image: &str, container_id: &str) -> Result<ResolvedRootfs, String> {
        let direct = Path::new(image);
        if direct.is_absolute() && direct.is_dir() {
            return Ok(ResolvedRootfs::Dir(direct.to_path_buf()));
        }
        if let Some(img) = self.cas_ref(image) {
            self.store.ensure_layers(&img)?;
            if self.overlay_ok {
                let view = self.store.new_view(container_id)?;
                return Ok(ResolvedRootfs::Overlay {
                    mountpoint: view.mountpoint,
                    lower: img
                        .layers
                        .iter()
                        .map(|l| self.store.layer_dir(&l.digest))
                        .collect(),
                    upper: view.upper,
                    work: view.work,
                });
            }
            return self
                .store
                .ensure_merged_fused(&img)
                .map(ResolvedRootfs::Dir);
        }
        let staged = self.images_dir.join(image);
        if staged.is_dir() {
            return Ok(ResolvedRootfs::Dir(staged));
        }
        Err(format!("builtin engine: image {image:?} not pulled"))
    }

    /// Presence-only check for `pull_image` (IfNotPresent semantics): no
    /// side effects, so nothing is created before the pull decision.
    fn image_present(&self, image: &str) -> bool {
        let direct = Path::new(image);
        (direct.is_absolute() && direct.is_dir())
            || self.cas_ref(image).is_some()
            || self.images_dir.join(image).is_dir()
    }

    /// The CAS pull record for `image`, if one exists: exact normalized
    /// reference first, then digest lookup for `repo@sha256:` spellings.
    fn cas_ref(&self, image: &str) -> Option<PulledImage> {
        let pr = super::image::parse_ref(image, self.puller.host()).ok()?;
        let key = super::image::reference::normalized(&pr);
        self.store
            .get_ref(&key)
            .or_else(|| self.store.find_ref_by_digest(&pr.digest?))
    }

    /// The image config body behind `image` (T4.13 M4, Q53): `None` for
    /// host-dir/staged images without a CAS record; the config blob is
    /// read from the CAS and parsed whenever the image was pulled.
    fn image_config_body(&self, image: &str) -> Result<Option<ConfigBody>, String> {
        let Some(img) = self.cas_ref(image) else {
            return Ok(None);
        };
        let blob = self.store.blob_path(&img.config.digest);
        let bytes = std::fs::read(&blob)
            .map_err(|e| format!("read image config {}: {e}", blob.display()))?;
        let parsed = super::image::parse_image_config(&bytes)?;
        Ok(Some(parsed.config))
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

    /// The SIGTERM grace for one container (T4.13 M4, Q53): the pod
    /// annotation `init-pro.io/stop-grace-seconds` overrides the default
    /// [`STOP_GRACE`], clamped to 1..=600s so a typo cannot wedge the
    /// sync loop. Unparseable/absent values keep the default.
    fn stop_grace(&self, id: &str) -> Duration {
        self.registry
            .pending(id)
            .and_then(|p| {
                p.pcfg
                    .annotations
                    .get("init-pro.io/stop-grace-seconds")
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .map(|s| Duration::from_secs(s.clamp(1, 600)))
            .unwrap_or(STOP_GRACE)
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
        // T4.13 M2: pulled CAS refs read as present so sync's IfNotPresent
        // short-circuits without a network round-trip.
        tags.extend(self.store.list_refs().into_iter().map(|r| r.reference));
        Ok(tags)
    }

    async fn pull_image(&self, image: &str) -> Result<(), String> {
        if image == self.sandbox_image {
            // The pause engine parks before exec — no rootfs needed.
            return Ok(());
        }
        if self.image_present(image) {
            return Ok(());
        }
        // T4.13 M2 (Q53/Q54): real registry-v2 pull into the CAS.
        self.puller.pull(image).await.map(|_| ())
    }

    /// T4.13 M5 image-delete face: drop the ref FILE (by normalized ref,
    /// or by manifest digest for `repo@sha256:` spellings), then GC —
    /// blobs/layers/merged dirs no surviving ref owns are swept; layers
    /// SHARED with a kept ref survive (dedupe by digest). Unknown image
    /// is `Ok(())` (idempotent, `crictl rmi` semantics). The caller owns
    /// the in-use question: deleting a ref an in-flight pod still needs
    /// frees its layers at the next GC.
    async fn remove_image(&self, image: &str) -> Result<(), String> {
        let Some(img) = self.cas_ref(image) else {
            return Ok(());
        };
        // T4.13 review follow-up (crictl rmi semantics): a live container
        // holds the shared layer dir as its overlayfs lowerdir — sweeping
        // it now would delete the dir under a running mount. Refuse until
        // the container is removed; matching goes through cas_ref so tag
        // and digest spellings of the same reference both count as in-use.
        for (id, used) in self.registry.images() {
            if self
                .cas_ref(&used)
                .is_some_and(|u| u.reference == img.reference)
            {
                return Err(format!(
                    "builtin engine: image {image} in use by container {id} (remove the container first)"
                ));
            }
        }
        self.store.remove_ref(&img.reference);
        self.store.gc()?;
        Ok(())
    }

    async fn list_sandboxes(&self) -> Result<Vec<SandboxView>, String> {
        // Q52: builtin pods share the host netns — a READY sandbox's IP is
        // the node IP (registry views start ip:None; overlaid here).
        Ok(self
            .registry
            .sandbox_views()
            .into_iter()
            .map(|mut sb| {
                if sb.state == "SANDBOX_READY" && sb.ip.is_none() {
                    sb.ip = Some(self.node_ip.clone());
                }
                sb
            })
            .collect())
    }

    async fn list_containers(&self) -> Result<Vec<ContainerView>, String> {
        Ok(self.registry.container_views())
    }

    async fn run_pod_sandbox(&self, cfg: &PodSandboxConfig) -> Result<String, String> {
        let id = new_id("sb-");
        let socket = self.socket_for(&id);
        // Q52: builtin pause sandbox = host netns (no fresh netns, no CNI).
        let child = launch::spawn_engine(&socket, true, true)
            .map_err(|e| format!("spawn pause engine: {e}"))?;
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
            image: self.sandbox_image.clone(),
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
            image: ccfg.image.image.clone(),
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
        let (rootfs, overlay, layer_dirs) = match self.resolve(&pending.ccfg.image.image, id)? {
            ResolvedRootfs::Dir(dir) => (dir.clone(), None, vec![dir]),
            ResolvedRootfs::Overlay {
                mountpoint,
                lower,
                upper,
                work,
            } => (
                mountpoint,
                Some(engine_proto::OverlaySpec {
                    lower: lower
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect(),
                    upper: upper.to_string_lossy().into_owned(),
                    work: work.to_string_lossy().into_owned(),
                }),
                // `/etc/passwd` reads happen per-layer (the view itself is
                // only mounted inside the engine child).
                lower,
            ),
        };
        // T4.13 M4 (Q53): the image config is the launch source of truth —
        // entrypoint/cmd/env/user/workingDir merge under the CRI request
        // (explicit pod command overrides; pod env appends last-wins).
        let img = self.image_config_body(&pending.ccfg.image.image)?;
        let resolve_named_user = named_user_lookup(&layer_dirs);
        let launch = spec::merge_launch(&pending.ccfg, img.as_ref(), Some(&resolve_named_user))?;
        let mut es = spec::container_spec(id, &rootfs, &pending, sandbox_netns, launch)?;
        // T4.13 M3 (Q53): the engine child mounts the overlay view over
        // `rootfs` before entering it; plain dirs keep `overlay: None`.
        es.overlay = overlay;
        let child = launch::spawn_engine(&socket, false, false)
            .map_err(|e| format!("spawn engine: {e}"))?;
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
        let grace = self.stop_grace(id);
        if let Some(ctrl) = self.registry.ctrl(id) {
            let _ = ctrl.send(Ctrl::Signal(15)).await;
            if self.wait_state(id, "CONTAINER_EXITED", grace).await {
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
        // T4.13 M3 (Q53): drop the container's overlay view scratch.
        self.store.remove_view(id);
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

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn temp_data_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "init-pro-engine-backend-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    pub(super) fn sandbox_entry(dir: &Path, id: &str, state: &str) -> Entry {
        Entry {
            id: id.to_string(),
            socket: dir.join("engine").join(format!("{id}.sock")),
            proc: Proc::Sandbox(None),
            state: state.to_string(),
            exit_code: None,
            pid: Some(7),
            netns: Some("/proc/7/ns/net".to_string()),
            name: id.to_string(),
            namespace: "default".to_string(),
            uid: String::new(),
            attempt: 0,
            labels: BTreeMap::new(),
            image: String::new(),
            pending: None,
            ctrl: None,
        }
    }

    /// Q52 node-IP overlay: READY sandboxes report the node IP, NOTREADY
    /// entries keep ip:None (no fake IPs for dead sandboxes).
    #[tokio::test]
    async fn ready_sandboxes_overlay_node_ip_notready_stay_none() {
        let dir = temp_data_dir("ip-overlay");
        std::fs::create_dir_all(&dir).unwrap();
        let backend = EngineBackend::new(
            &dir,
            "registry.local/pause:0",
            "192.0.2.7",
            "127.0.0.1:15000",
        )
        .unwrap();
        backend
            .registry
            .insert(sandbox_entry(&dir, "sb-ready", "SANDBOX_READY"));
        backend
            .registry
            .insert(sandbox_entry(&dir, "sb-dead", "SANDBOX_NOTREADY"));

        let views = backend.list_sandboxes().await.expect("list sandboxes");
        let ready = views.iter().find(|v| v.id == "sb-ready").unwrap();
        let dead = views.iter().find(|v| v.id == "sb-dead").unwrap();
        assert_eq!(ready.ip.as_deref(), Some("192.0.2.7"));
        assert_eq!(dead.ip, None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// T4.13 `remove_image` semantics (Q53 + review follow-up): split out
/// of backend.rs at the 800-line rule, same shape as `image/unpack.rs`.
#[cfg(test)]
mod rm_image_tests;
