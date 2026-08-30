//! Pure reconcile core (TODO **T4.2**): [`plan`] diffs the desired pod set
//! against a CRI [`Snapshot`] and emits an ordered [`Action`] list that
//! [`execute`] applies through the [`CriBackend`] trait (Q26/Q27 seam).
//! `plan` is pure (tested in `tests/sync_plan.rs`) and keeps ONE forward
//! step per pod per cycle, so recreate follows teardown next cycle.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use runtime::cri_json::{
    container_labels, pod_labels, ContainerMetadata, ImageSpec, SandboxMetadata, LABEL_POD_NAME,
    LABEL_POD_NAMESPACE,
};
use runtime::{ContainerConfig, PodSandboxConfig};

use crate::cri_backend::{ContainerView, CriBackend, SandboxView};
use crate::objects::{ContainerSpec, PodView};
use crate::probes::PodProbeState;

/// Observed CRI state at one instant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub images: Vec<String>,
    pub sandboxes: Vec<SandboxView>,
    pub containers: Vec<ContainerView>,
}

/// One CRI mutation. Ordered by [`plan`], applied by [`execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `crictl pull` (planned per the image pull policy — Sprint 20 S3:
    /// Always re-pulls, IfNotPresent pulls only when absent, Never never).
    EnsureImage(String),
    /// `crictl runp` with the pod's sandbox config.
    CreateSandbox {
        pod_key: String,
        cfg: PodSandboxConfig,
    },
    /// `crictl create` + `crictl start` (fresh container). The config is
    /// boxed to keep the variant small (ContainerConfig carries mounts).
    CreateStartContainer {
        pod_key: String,
        sandbox_id: String,
        ccfg: Box<ContainerConfig>,
        pcfg: PodSandboxConfig,
    },
    /// `crictl start` an existing stopped container.
    StartContainer { pod_key: String, id: String },
    /// `crictl stop` + `crictl rm` (already-gone tolerated).
    RemoveContainer { pod_key: String, id: String },
    /// `crictl stopp` + `crictl rmp` (already-gone tolerated).
    StopRemoveSandbox { pod_key: String, id: String },
}

impl Action {
    /// The pod this action targets (`None` for image-level pulls).
    pub fn pod_key(&self) -> Option<&str> {
        match self {
            Action::EnsureImage(_) => None,
            Action::CreateSandbox { pod_key, .. }
            | Action::CreateStartContainer { pod_key, .. }
            | Action::StartContainer { pod_key, .. }
            | Action::RemoveContainer { pod_key, .. }
            | Action::StopRemoveSandbox { pod_key, .. } => Some(pod_key),
        }
    }
}

/// Diff desired vs observed -> the action list for this cycle. `probes`
/// carries the Sprint 20 S1 probe outcomes: a liveness failure restarts
/// the container (restartPolicy permitting) even while it is RUNNING.
pub fn plan(
    desired: &BTreeMap<String, PodView>,
    snap: &Snapshot,
    sandbox_image: &str,
    log_root: &Path,
    probes: &PodProbeState,
) -> Vec<Action> {
    let mut actions = Vec::new();
    // Index observations: sandboxes by "ns/name" (garbage labels skipped),
    // containers by (pod key, container name).
    let mut sb_by_pod: BTreeMap<String, Vec<&SandboxView>> = BTreeMap::new();
    for sb in &snap.sandboxes {
        if sb.namespace.is_empty() || sb.name.is_empty() {
            continue;
        }
        sb_by_pod
            .entry(format!("{}/{}", sb.namespace, sb.name))
            .or_default()
            .push(sb);
    }
    let mut ct_by_pod: BTreeMap<String, Vec<&ContainerView>> = BTreeMap::new();
    for c in &snap.containers {
        if let Some(key) = container_pod_key(c, &snap.sandboxes) {
            ct_by_pod.entry(key).or_default().push(c);
        }
    }

    let mut seen = BTreeSet::new();
    for (key, view) in desired {
        seen.insert(key.clone());
        let observed = sb_by_pod.get(key).cloned().unwrap_or_default();
        let pod_containers = ct_by_pod.get(key).cloned().unwrap_or_default();
        let (ours, stale): (Vec<_>, Vec<_>) = observed
            .into_iter()
            .partition(|sb| sb.uid.is_empty() || sb.uid == view.uid);
        for sb in &stale {
            teardown(&mut actions, key, sb, &pod_containers);
        }
        if view.deleted {
            for sb in &ours {
                teardown(&mut actions, key, sb, &pod_containers);
            }
            continue;
        }
        if !stale.is_empty() {
            // One forward step per pod per cycle: recreate the pod on the
            // cycle after the stale (wrong-uid) sandbox is gone.
            continue;
        }
        let ready: Vec<_> = ours
            .iter()
            .filter(|sb| sb.state == "SANDBOX_READY")
            .collect();
        if ready.is_empty() {
            // Sandbox absent or unhealthy. Unhealthy first gets torn down
            // (single step; recreation happens the cycle after). Absent
            // means: ensure images + create the sandbox now.
            if !ours.is_empty() {
                for sb in &ours {
                    teardown(&mut actions, key, sb, &pod_containers);
                }
                continue;
            }
            for c in &pod_containers {
                actions.push(Action::RemoveContainer {
                    pod_key: key.clone(),
                    id: c.id.clone(),
                });
            }
            // The sandbox image (pause) is always pulled with IfNotPresent
            // semantics: upstream pulls the sandbox image on demand and no
            // user policy applies (Sprint 20 S3 keeps that behavior).
            ensure_image(&mut actions, &snap.images, sandbox_image, "IfNotPresent");
            for spec in &view.containers {
                ensure_image(
                    &mut actions,
                    &snap.images,
                    &spec.image,
                    &spec.image_pull_policy,
                );
            }
            actions.push(Action::CreateSandbox {
                pod_key: key.clone(),
                cfg: sandbox_cfg(view, log_root),
            });
            continue;
        }
        // Healthy sandbox: keep the first READY one, tear down extras.
        let keep = ready[0];
        for sb in ready
            .into_iter()
            .skip(1)
            .chain(ours.iter().filter(|s| s.state != "SANDBOX_READY"))
        {
            teardown(&mut actions, key, sb, &pod_containers);
        }
        // Restart policy drives the per-container branch (Sprint 20 S1):
        // Always (default) recreates anything not RUNNING plus liveness
        // failures; OnFailure only after a non-zero exit or a liveness
        // failure (exited-0 stays); Never leaves dead containers as-is,
        // only first creation happens. Recreate emits Remove + Create in
        // one cycle (ONE forward step per pod per cycle).
        for spec in &view.containers {
            let observed: Vec<_> = pod_containers
                .iter()
                .filter(|c| c.name == spec.name)
                .collect();
            let running = observed.iter().any(|c| c.state == "CONTAINER_RUNNING");
            let liveness_failed = probes.liveness_failed(key, &spec.name);
            let recreate = match view.restart_policy.as_str() {
                "Never" => observed.is_empty(),
                "OnFailure" => {
                    if running {
                        liveness_failed
                    } else {
                        let completed = observed
                            .iter()
                            .any(|c| c.state == "CONTAINER_EXITED" && c.exit_code == 0);
                        !completed || liveness_failed
                    }
                }
                _ => !running || liveness_failed,
            };
            if !recreate {
                continue;
            }
            let attempt = observed
                .iter()
                .map(|c| c.attempt)
                .max()
                .map_or(0, |a| a + 1);
            for c in observed {
                actions.push(Action::RemoveContainer {
                    pod_key: key.clone(),
                    id: c.id.clone(),
                });
            }
            ensure_image(
                &mut actions,
                &snap.images,
                &spec.image,
                &spec.image_pull_policy,
            );
            actions.push(Action::CreateStartContainer {
                pod_key: key.clone(),
                sandbox_id: keep.id.clone(),
                ccfg: Box::new(container_cfg(view, spec, attempt, log_root)),
                pcfg: sandbox_cfg(view, log_root),
            });
        }
    }

    // Observed sandboxes whose pod is gone (deleted/unscheduled) tear down.
    for (key, sbs) in &sb_by_pod {
        if seen.contains(key) {
            continue;
        }
        for sb in sbs {
            let cs: Vec<&ContainerView> = snap
                .containers
                .iter()
                .filter(|c| c.sandbox_id == sb.id)
                .collect();
            teardown(&mut actions, key, sb, &cs);
        }
    }
    actions
}

fn sandbox_cfg(view: &PodView, log_root: &Path) -> PodSandboxConfig {
    PodSandboxConfig {
        metadata: SandboxMetadata {
            name: view.name.clone(),
            namespace: view.namespace.clone(),
            uid: view.uid.clone(),
            attempt: 0,
        },
        labels: pod_labels(&view.name, &view.namespace, &view.uid),
        annotations: BTreeMap::new(),
        // Hostname passthrough (Sprint 20 S4, T4.2 Scope B): v1 uses the
        // pod name verbatim so containers stop inheriting the sandbox
        // host's hostname. Full upstream FQDN semantics
        // (`spec.hostname`/`spec.subdomain` composition) are deferred to
        // a later sprint.
        hostname: view.name.clone(),
        log_directory: log_root.join(&view.uid).to_string_lossy().into_owned(),
    }
}

fn container_cfg(
    view: &PodView,
    spec: &ContainerSpec,
    attempt: u32,
    log_root: &Path,
) -> ContainerConfig {
    ContainerConfig {
        metadata: ContainerMetadata {
            name: spec.name.clone(),
            attempt,
        },
        image: ImageSpec {
            image: spec.image.clone(),
        },
        command: spec.command.clone(),
        args: spec.args.clone(),
        envs: Vec::new(),
        labels: container_labels(&view.name, &view.namespace, &view.uid, &spec.name),
        log_path: format!("{}.{attempt}.log", spec.name),
        // Sprint 20 S2 (T4.2 Scope B): pod volumes become CRI host mounts
        // (configMap/secret read-only); unknown volume names are skipped
        // so the CRI create fails and the cycle retries after the volume
        // pass converges.
        mounts: crate::volumes::resolve_mounts(spec, &view.volumes, &view.uid, log_root),
    }
}

/// Teardown order: every container stop+rm first, then the sandbox stopp+rmp.
pub(crate) fn teardown(
    actions: &mut Vec<Action>,
    pod_key: &str,
    sb: &SandboxView,
    pod_containers: &[&ContainerView],
) {
    let mut mine: Vec<&ContainerView> = pod_containers
        .iter()
        .filter(|c| c.sandbox_id == sb.id)
        .copied()
        .collect();
    mine.sort_by(|a, b| a.id.cmp(&b.id));
    for c in mine {
        actions.push(Action::RemoveContainer {
            pod_key: pod_key.to_string(),
            id: c.id.clone(),
        });
    }
    actions.push(Action::StopRemoveSandbox {
        pod_key: pod_key.to_string(),
        id: sb.id.clone(),
    });
}

/// Sprint 20 S3 (T4.2 Scope B): decide whether one image needs a pull this
/// cycle. `policy` is the RESOLVED policy from the pod spec (see
/// `objects::resolve_image_pull_policy`):
/// - `Always` — re-pull every sync cycle (upstream kubelet behavior).
/// - `Never` — never pull; create proceeds and fails at the CRI layer if
///   the image is absent, and the cycle retries (v1 behavior).
/// - `IfNotPresent` (default for pinned tags) — pull only when absent.
///   This is the airgap fallback (Q27): locally imported OCI images (via
///   `ctr -n k8s.io images import`) are found here without any network.
///
/// `imagePullSecrets` is EXPLICITLY DEFERRED (Sprint 20 / T4.2 card):
/// pulls are unauthenticated until a later sprint.
fn ensure_image(actions: &mut Vec<Action>, present: &[String], image: &str, policy: &str) {
    if image.is_empty() {
        return;
    }
    match policy {
        "Always" => {}
        "Never" => return,
        // IfNotPresent (and any unknown value falls back to it): pull only
        // when absent.
        _ => {
            if present.iter().any(|t| t == image) {
                return;
            }
        }
    }
    actions.push(Action::EnsureImage(image.to_string()));
}

/// Pod key of a container: `io.kubernetes.*` labels first, else inherit the
/// identity of its sandbox (garbage containers without either -> None).
fn container_pod_key(c: &ContainerView, sandboxes: &[SandboxView]) -> Option<String> {
    let ns = c.labels.get(LABEL_POD_NAMESPACE).map(String::as_str);
    let name = c.labels.get(LABEL_POD_NAME).map(String::as_str);
    if let (Some(ns), Some(name)) = (ns, name) {
        if !ns.is_empty() && !name.is_empty() {
            return Some(format!("{ns}/{name}"));
        }
    }
    let sb = sandboxes.iter().find(|s| s.id == c.sandbox_id)?;
    if sb.namespace.is_empty() || sb.name.is_empty() {
        return None;
    }
    Some(format!("{}/{}", sb.namespace, sb.name))
}

/// Bound for the whole shutdown teardown ([`drain_sandboxes`]): generous
/// enough for several retry sweeps of `crictl stop/rm/stopp/rmp` round-trips,
/// tight enough that a wedged CRI cannot hang the agent's drain (T4.1
/// follow-up, Sprint 28).
pub const SANDBOX_DRAIN_BUDGET: Duration = Duration::from_secs(15);

/// Pause between drain sweeps when the previous sweep left sandboxes behind
/// (crictl/CRI state needs a beat to settle before a retry).
const DRAIN_SWEEP_BACKOFF: Duration = Duration::from_millis(250);

/// Shutdown teardown (T4.1 follow-up, Sprint 28): drive CRI to zero
/// sandboxes so no shim outlives the containerd daemon the agent is about to
/// stop. Sweeps until CRI reports none or `budget` expires:
///
/// 1. stop + remove every container (`task`) of each sandbox FIRST — a
///    RUNNING sandbox makes CRI `StopPodSandbox` stop live containers inside
///    the request, which blows crictl's 2s deadline (`DeadlineExceeded`);
///    the sync path never sees this because `plan` tears containers down a
///    cycle before the sandbox.
/// 2. then `stopp` + `rmp` the sandbox itself.
///
/// Already-gone objects are tolerated (NotFound); transient rejections heal
/// on the next sweep. The budget bounds the whole pass — on expiry it logs
/// and returns; the caller stops containerd regardless, after THIS task is
/// joined. Returns the number of sandbox attempts issued across sweeps.
pub async fn drain_sandboxes(cri: &dyn CriBackend, budget: Duration) -> usize {
    let sweeps = async {
        let mut attempts = 0usize;
        loop {
            let sandboxes = match cri.list_sandboxes().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        target: "init-pro",
                        error = %e,
                        "kubelet drain: CRI sandbox list failed"
                    );
                    return attempts;
                }
            };
            if sandboxes.is_empty() {
                return attempts;
            }
            let containers = cri.list_containers().await.unwrap_or_default();
            let mut rejected = false;
            for sb in &sandboxes {
                attempts += 1;
                for c in containers.iter().filter(|c| c.sandbox_id == sb.id) {
                    if let Err(e) = cri.stop_container(&c.id).await {
                        rejected = true;
                        tracing::debug!(
                            target: "init-pro",
                            container = %c.id,
                            error = %e,
                            "kubelet drain: container stop rejected (retried)"
                        );
                    }
                    if let Err(e) = cri.remove_container(&c.id).await {
                        rejected = true;
                        tracing::debug!(
                            target: "init-pro",
                            container = %c.id,
                            error = %e,
                            "kubelet drain: container rm rejected (retried)"
                        );
                    }
                }
                if let Err(e) = cri.stop_pod_sandbox(&sb.id).await {
                    rejected = true;
                    tracing::debug!(
                        target: "init-pro",
                        sandbox = %sb.id,
                        error = %e,
                        "kubelet drain: stopp rejected (retried)"
                    );
                }
                if let Err(e) = cri.remove_pod_sandbox(&sb.id).await {
                    rejected = true;
                    tracing::debug!(
                        target: "init-pro",
                        sandbox = %sb.id,
                        error = %e,
                        "kubelet drain: rmp rejected (retried)"
                    );
                }
            }
            if rejected {
                tokio::time::sleep(DRAIN_SWEEP_BACKOFF).await;
            }
        }
    };
    match tokio::time::timeout(budget, sweeps).await {
        Ok(n) => n,
        Err(_) => {
            tracing::warn!(
                target: "init-pro",
                budget_ms = budget.as_millis() as u64,
                "kubelet drain: sandbox teardown budget exceeded"
            );
            0
        }
    }
}
