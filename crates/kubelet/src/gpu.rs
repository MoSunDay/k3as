//! GPU plane for the kubelet: inventory -> durable ledger -> node
//! publication -> container env injection (TODO **T7.2**, decision
//! **Q61**; fake-scope here, real-hardware closure stays OPEN per GPU6).
//!
//! Layering (pure-functional; plain data + a thin state holder):
//! - [`inventory`] — `--gpu-inventory` parsing + best-effort detection.
//! - [`ledger`] — the exclusive whole-device ledger file + its invariants.
//! - [`status`] — node eligibility, capacity/allocatable publication and
//!   the frozen `status.initProRuntime` contract.
//! - this module — [`GpuRuntime`] (ledger + inventory cache, shared by
//!   the sync + node tasks behind one mutex), the per-cycle
//!   [`prepare`] allocation pass and the teardown-completion
//!   [`release_finished`] pass.

pub mod inventory;
pub mod ledger;
pub mod status;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use common::gpu::is_gpu_resource;
use common::parse_quantity;

use crate::engine::{HOST_DIRECT_ROOT, ROOTFS_ANNOTATION};
use crate::objects::PodView;
use crate::sync;
use crate::{env_fetch, runner::KubeletConfig};

/// Ledger file location relative to the agent data dir (Q61).
pub fn ledger_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("engine/gpu-ledger.json")
}

/// The shared per-agent GPU state (ledger + inventory cache). Guarded by
/// a std Mutex: critical sections are tiny and never held across an
/// await (detection runs unlocked; see [`refresh_totals`]).
pub struct GpuRuntime {
    pub ledger: ledger::GpuLedger,
    source: inventory::GpuInventorySource,
    cache: inventory::TotalsCache,
}

impl GpuRuntime {
    pub fn open(data_dir: &Path, source: inventory::GpuInventorySource) -> Self {
        GpuRuntime {
            ledger: ledger::GpuLedger::open(ledger_path(data_dir)),
            source,
            cache: inventory::TotalsCache::new(),
        }
    }

    /// The definitive totals under the configured source (pure view of
    /// the cache; `NotMeasured` only possible for `auto` before the
    /// first heartbeat detection).
    pub fn totals(&self) -> inventory::Totals {
        inventory::resolve_totals(&self.source, self.cache.peek())
    }

    fn source_kind(&self) -> &inventory::GpuInventorySource {
        &self.source
    }

    /// Store one detection result (called once per heartbeat — the Q61
    /// "cache per heartbeat" cadence; `ttl` is handed to the cache so a
    /// defensive second call inside one heartbeat is a no-op).
    fn cache_refresh(&mut self, detected: BTreeMap<String, u64>, ttl: Duration) {
        let now = std::time::Instant::now();
        let _ = self.cache.refresh(ttl, now, || detected.clone());
    }
}

/// The runner's shared GPU-plane handle (T7.2, Q61).
pub type GpuHandle = Arc<Mutex<GpuRuntime>>;

/// Lock helper (poison-tolerant: a panicked critical section never takes
/// the whole kubelet down).
pub fn lock(rt: &Arc<Mutex<GpuRuntime>>) -> MutexGuard<'_, GpuRuntime> {
    rt.lock().unwrap_or_else(|p| p.into_inner())
}

/// Refresh the auto-mode detection WITHOUT holding the lock across the
/// (async) detection: check the source under lock, detect unlocked,
/// store under lock. Static sources (`none`/`fake`) never detect — their
/// totals resolve without any host access.
pub async fn refresh_totals(rt: &Arc<Mutex<GpuRuntime>>, ttl: Duration) -> inventory::Totals {
    if !matches!(lock(rt).source_kind(), inventory::GpuInventorySource::Auto) {
        return lock(rt).totals();
    }
    let detected = inventory::detect_host_gpus().await;
    let mut g = lock(rt);
    g.cache_refresh(detected, ttl);
    g.totals()
}

/// One cycle's allocation output (plain data): devices pinned per
/// (pod key, container name). Guard/exhaustion failures do NOT appear
/// here — `prepare` writes them as create-blocking errors into the env
/// table so the existing CreateContainerConfigError event machinery
/// reports them (upstream error class, deduped, no new event path).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuPrepared {
    pub devices: BTreeMap<(String, String), (String, Vec<u64>)>,
}

impl GpuPrepared {
    /// The (resource, devices) pinned for one container, if any.
    pub fn devices_for(&self, pod_key: &str, container: &str) -> Option<(&str, &[u64])> {
        self.devices
            .get(&(pod_key.to_string(), container.to_string()))
            .map(|(r, d)| (r.as_str(), d.as_slice()))
    }
}

/// Whole-GPU requests of one pod view: container name -> (resource,
/// count). Non-integral or negative quantities are INVALID (deterministic
/// rejection; admission-side validation is another agent's scope).
pub fn pod_view_gpu_wants(view: &PodView) -> Vec<(String, String, u64)> {
    let mut out = Vec::new();
    for c in &view.containers {
        for (resource, qty) in &c.resources.requests {
            if !is_gpu_resource(resource) {
                continue;
            }
            if let Some(n) = parse_quantity(qty).filter(|n| *n >= 1.0 && n.fract() == 0.0) {
                out.push((c.name.clone(), resource.clone(), n as u64));
            }
        }
    }
    out
}

/// Does the view request any GPU resource (invalid quantities included)?
pub(crate) fn view_uses_gpu(view: &PodView) -> bool {
    view.containers
        .iter()
        .any(|c| c.resources.requests.keys().any(|r| is_gpu_resource(r)))
}

/// The per-cycle GPU pass BEFORE `plan` (T7.2, Q61):
/// 1. reconcile the ledger against the live desired set (grace-prune);
/// 2. guard every GPU/host-direct pod through [`sync::gpu_backend_guard`]
///    (containerd rejection — Q60/Q61, no fallback);
/// 3. allocate whole devices transactionally (GPU2/GPU3) and record the
///    pinned mapping for env injection;
/// 4. surface guard/over-allocation failures as env-table errors so the
///    create blocks and the existing event pass reports why.
pub fn prepare(
    rt: &mut GpuRuntime,
    cfg: &KubeletConfig,
    desired: &BTreeMap<String, crate::objects::PodView>,
    env_table: &mut env_fetch::EnvTable,
    now_unix: u64,
) -> GpuPrepared {
    let capable = status::gpu_capable(cfg.engine_builtin, cfg.builtin_rootfs, cfg.engine_builtin);
    let live: std::collections::BTreeSet<String> =
        desired.values().map(|v| v.uid.clone()).collect();
    if rt.ledger.reconcile(&live, now_unix) {
        let _ = rt.ledger.save();
    }
    let totals = match rt.totals() {
        inventory::Totals::Measured(t) => t,
        // Auto mode before the first heartbeat detection: never block on
        // an unmeasured node — allocation starts once totals are known.
        inventory::Totals::NotMeasured => return GpuPrepared::default(),
    };
    let mut prepared = GpuPrepared::default();
    for (key, view) in desired {
        if view.deleted || !view_uses_gpu(view) {
            continue;
        }
        let wants = pod_view_gpu_wants(view);
        if wants.is_empty() {
            // GPU keys present but no valid integral request: invalid pod.
            block_pod(
                env_table,
                key,
                view,
                &wants,
                "GPU requests must be positive whole-device integers (e.g. \"1\"; Q61)",
            );
            continue;
        }
        if let Some(reason) = sync::gpu_backend_guard(view, cfg.engine_builtin, capable) {
            block_pod(env_table, key, view, &wants, &reason);
            continue;
        }
        match rt.ledger.allocate_pod(&view.uid, &wants, &totals, now_unix) {
            Ok(per_container) => {
                for (container, devices) in per_container {
                    let resource = wants
                        .iter()
                        .find(|(c, _, _)| c == &container)
                        .map(|(_, r, _)| r.clone())
                        .unwrap_or_default();
                    prepared
                        .devices
                        .insert((key.clone(), container), (resource, devices));
                }
                let _ = rt.ledger.save();
            }
            Err(e) => block_pod(env_table, key, view, &wants, &e),
        }
    }
    prepared
}

/// Write one create-blocking error per GPU-requesting container (never
/// clobbering an earlier env-resolution error — that one reports first).
fn block_pod(
    env_table: &mut env_fetch::EnvTable,
    key: &str,
    view: &PodView,
    wants: &[(String, String, u64)],
    reason: &str,
) {
    let containers: Vec<String> = if wants.is_empty() {
        view.containers
            .iter()
            .filter(|c| c.resources.requests.keys().any(|r| is_gpu_resource(r)))
            .map(|c| c.name.clone())
            .collect()
    } else {
        wants.iter().map(|(c, _, _)| c.clone()).collect()
    };
    for container in containers {
        let outcome = env_table
            .entry(key.to_string())
            .or_default()
            .entry(container)
            .or_insert_with(|| env_fetch::EnvOutcome {
                envs: Vec::new(),
                error: None,
            });
        if outcome.error.is_none() {
            outcome.error = Some(reason.to_string());
        }
    }
}

/// The teardown-completion pass AFTER `execute` (GPU4): release the
/// ledger entries of every pod whose containers/sandbox teardown the
/// kubelet finished (gone from the desired set AND from the observed
/// snapshot), appending the `teardown-complete` audit records. Uses the
/// PRE-execute snapshot deliberately (same race-free one-cycle-later
/// contract as the volume cleanup pass).
pub fn release_finished(
    rt: &mut GpuRuntime,
    desired: &BTreeMap<String, PodView>,
    snap: &crate::sync::Snapshot,
    now_unix: u64,
) -> usize {
    let done = sync::completed_gpu_teardowns(&rt.ledger.allocated_uids(), desired, snap);
    let mut released = 0;
    for uid in &done {
        if rt.ledger.release(uid, now_unix, "teardown-complete") {
            released += 1;
        }
    }
    if released > 0 {
        let _ = rt.ledger.save();
    }
    released
}

/// Lock + [`prepare`] in one call — the runner's per-cycle entry (the
/// lock is taken and dropped here; NEVER held across an await).
pub fn prepare_cycle(
    gpu: &Arc<Mutex<GpuRuntime>>,
    cfg: &KubeletConfig,
    desired: &BTreeMap<String, PodView>,
    env_table: &mut env_fetch::EnvTable,
    now_unix: u64,
) -> GpuPrepared {
    prepare(&mut lock(gpu), cfg, desired, env_table, now_unix)
}

/// Lock + [`release_finished`] in one call (post-execute pass).
pub fn release_finished_cycle(
    gpu: &Arc<Mutex<GpuRuntime>>,
    desired: &BTreeMap<String, PodView>,
    snap: &crate::sync::Snapshot,
    now_unix: u64,
) -> usize {
    release_finished(&mut lock(gpu), desired, snap, now_unix)
}

/// Publish helper for the node heartbeat: prune the released-window and
/// decorate one Node `.status` in place (GPU5 wholesale-rebuild safety).
pub fn publish_snapshot(
    rt: &mut GpuRuntime,
    cfg: &KubeletConfig,
    node_status: &mut serde_json::Value,
    now_unix: u64,
) {
    let capable = status::gpu_capable(cfg.engine_builtin, cfg.builtin_rootfs, cfg.engine_builtin);
    if rt.ledger.prune_released(now_unix) {
        let _ = rt.ledger.save();
    }
    let totals = match rt.totals() {
        inventory::Totals::Measured(t) => t,
        inventory::Totals::NotMeasured => BTreeMap::new(),
    };
    status::decorate_node_status(node_status, &totals, capable, &rt.ledger.state);
}

/// Host-direct annotation predicate over a pod view (Q60 literal `/`).
pub fn view_is_host_direct(view: &PodView) -> bool {
    view.annotations.get(ROOTFS_ANNOTATION).map(String::as_str) == Some(HOST_DIRECT_ROOT)
}

// Re-exports: the flag value type + parse used by the CLI plumbing.
pub use inventory::{parse_inventory, GpuInventorySource};

#[cfg(test)]
#[path = "gpu_tests.rs"]
mod tests;
