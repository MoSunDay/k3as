//! Persistent whole-GPU exclusive allocation ledger (TODO **T7.2**,
//! decision **Q61**; gates **GPU2**/**GPU3**/**GPU4**).
//!
//! One durable JSON file at `<data_dir>/engine/gpu-ledger.json` (atomic
//! tmp+rename writes) keyed by **Pod UID + container name**, recording
//! the vendor resource and the physical device indices it owns. Pure
//! functions over plain data: [`GpuLedger`] is a thin state holder
//! (path + [`LedgerState`]); every decision function is pure and
//! unit-tested without hardware.
//!
//! Invariants:
//! - **GPU2 exclusivity** — one device lives in at most one entry at a
//!   time; allocation is transactional per pod (all containers or none,
//!   rolled back on any failure), so double booking is structurally
//!   impossible, not just checked.
//! - **GPU3 stability** — re-allocating an existing (UID, container,
//!   resource) entry returns the SAME devices across sync retries and
//!   agent restarts (the file is reloaded on boot).
//! - **GPU4 release-on-teardown** — entries survive pod deletion; only
//!   [`GpuLedger::release`] (teardown completion) or the grace-based
//!   [`GpuLedger::reconcile`] (pod gone from the store) frees devices.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Ledger file format version.
pub const LEDGER_VERSION: u32 = 1;
/// Retention for `released` audit records: the most recent 10 minutes.
pub const RELEASED_RETENTION_SECS: u64 = 600;
/// Grace before entries whose pod vanished from the store are pruned
/// (crash/retry loops of LIVE pods never hit this — only store absence
/// plus elapsed time does).
pub const RECONCILE_GRACE_SECS: u64 = 300;

/// One container's pinned devices: vendor resource + physical indices +
/// the unix time the entry was (re)created (grace input).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationEntry {
    pub resource: String,
    pub devices: Vec<u64>,
    pub since: u64,
}

/// One teardown-completion audit record (projected into
/// `status.initProRuntime.released`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedRecord {
    pub pod_uid: String,
    /// RFC3339 (UTC) release time — the wire-visible timestamp.
    pub at: String,
    /// Same instant as unix seconds (retention math; internal only).
    pub at_unix: u64,
    pub reason: String,
}

/// The whole durable ledger state (serde wire shape == file shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerState {
    pub version: u32,
    /// pod UID -> container name -> pinned devices.
    pub allocations: BTreeMap<String, BTreeMap<String, AllocationEntry>>,
    #[serde(default)]
    pub released: Vec<ReleasedRecord>,
}

impl Default for LedgerState {
    fn default() -> Self {
        LedgerState {
            version: LEDGER_VERSION,
            allocations: BTreeMap::new(),
            released: Vec::new(),
        }
    }
}

impl LedgerState {
    /// Devices currently held for `resource` (across all pods) — the
    /// allocatable publication input (pure fold over the state).
    pub fn allocated_count(&self, resource: &str) -> u64 {
        self.allocations
            .values()
            .flat_map(|m| m.values())
            .filter(|e| e.resource == resource)
            .map(|e| e.devices.len() as u64)
            .sum()
    }
}

/// Thin state holder: the file path plus the in-memory state.
#[derive(Debug)]
pub struct GpuLedger {
    path: PathBuf,
    pub state: LedgerState,
}

/// One container's allocation want: (container name, resource, count).
pub type Want = (String, String, u64);

impl GpuLedger {
    /// Open (or start) the ledger at `path`. A missing or corrupt file is
    /// NOT fatal (best-effort: an empty ledger starts fresh — Q61 keeps
    /// the agent alive; the warning is audible).
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|body| serde_json::from_str::<LedgerState>(&body).ok())
            .unwrap_or_default();
        GpuLedger { path, state }
    }

    /// Persist atomically: write `<path>.tmp`, fsync-free rename over the
    /// target (the tmp+rename pair keeps readers from ever seeing a torn
    /// file; the parent dir is created on demand).
    pub fn save(&self) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(&self.state)
            .map_err(|e| format!("serialize ledger: {e}"))?;
        std::fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), self.path.display()))
    }

    /// Every pod UID currently holding devices.
    pub fn allocated_uids(&self) -> Vec<String> {
        self.state.allocations.keys().cloned().collect()
    }

    /// Devices currently held for `resource` (across all pods).
    pub fn allocated_count(&self, resource: &str) -> u64 {
        self.state.allocated_count(resource)
    }

    /// The pinned entry for one (UID, container), if allocated.
    pub fn entry(&self, pod_uid: &str, container: &str) -> Option<&AllocationEntry> {
        self.state.allocations.get(pod_uid)?.get(container)
    }

    /// Free device indices for `resource` inside `0..total` (pure view).
    pub fn free_devices(&self, resource: &str, total: u64) -> Vec<u64> {
        let held: BTreeSet<u64> = self
            .state
            .allocations
            .values()
            .flat_map(|m| m.values())
            .filter(|e| e.resource == resource)
            .flat_map(|e| e.devices.iter().copied())
            .collect();
        (0..total).filter(|i| !held.contains(i)).collect()
    }

    /// Transactionally allocate every want of one pod (GPU2/GPU3):
    /// existing (UID, container, resource) entries are REUSED verbatim
    /// (stability); new wants take first-free devices; any failure rolls
    /// back the entries created by THIS call (reused ones stay put) and
    /// returns an actionable error. Devices live in `0..totals[resource]`.
    pub fn allocate_pod(
        &mut self,
        pod_uid: &str,
        wants: &[Want],
        totals: &BTreeMap<String, u64>,
        now_unix: u64,
    ) -> Result<BTreeMap<String, Vec<u64>>, String> {
        let mut made: Vec<String> = Vec::new();
        let mut out: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for (container, resource, count) in wants {
            if !common::gpu::is_gpu_resource(resource) {
                return Err(format!(
                    "unknown GPU resource {resource:?} (expected one of {:?})",
                    common::gpu::GPU_RESOURCES
                ));
            }
            if *count == 0 {
                continue;
            }
            let total = totals.get(resource).copied().unwrap_or(0);
            // GPU3: same UID+container+resource keeps its devices verbatim
            // (count is the guaranteed minimum; a shrunk request keeps the
            // wider booking until teardown — never a double book).
            if let Some(existing) = self.entry(pod_uid, container) {
                if existing.resource != *resource {
                    return Err(format!(
                        "container {container} already holds {} devices; \
                         changing the GPU resource in place is not supported",
                        existing.resource
                    ));
                }
                if existing.devices.len() as u64 >= *count {
                    out.insert(container.clone(), existing.devices.clone());
                    continue;
                }
            }
            let mut devices: Vec<u64> = self
                .entry(pod_uid, container)
                .map(|e| e.devices.clone())
                .unwrap_or_default();
            let need = *count - devices.len() as u64;
            let free = self.free_devices(resource, total);
            if free.len() < need as usize {
                let err = format!(
                    "insufficient free {resource} devices on this node \
                     (need {count}, {} free of {total} total; whole devices only, Q61)",
                    free.len()
                );
                self.rollback(pod_uid, &made);
                return Err(err);
            }
            devices.extend(free.into_iter().take(need as usize));
            let entry = AllocationEntry {
                resource: resource.clone(),
                devices: devices.clone(),
                since: now_unix,
            };
            self.state
                .allocations
                .entry(pod_uid.to_string())
                .or_default()
                .insert(container.clone(), entry);
            made.push(container.clone());
            out.insert(container.clone(), devices);
        }
        Ok(out)
    }

    /// Undo the entries created by one failed `allocate_pod` call (the
    /// released list is NOT touched — these devices were never visible).
    fn rollback(&mut self, pod_uid: &str, made: &[String]) {
        if made.is_empty() {
            return;
        }
        if let Some(per_pod) = self.state.allocations.get_mut(pod_uid) {
            for container in made {
                per_pod.remove(container);
            }
            if per_pod.is_empty() {
                self.state.allocations.remove(pod_uid);
            }
        }
    }

    /// GPU4: release every device of `pod_uid` — called ONLY when the
    /// kubelet finished tearing the pod's containers down. Appends an
    /// audit record and prunes the retention window. Returns true when
    /// something was actually released.
    pub fn release(&mut self, pod_uid: &str, now_unix: u64, reason: &str) -> bool {
        let Some(per_pod) = self.state.allocations.remove(pod_uid) else {
            return false;
        };
        if per_pod.is_empty() {
            return false;
        }
        self.state.released.push(ReleasedRecord {
            pod_uid: pod_uid.to_string(),
            at: common::time::now_rfc3339(now_unix),
            at_unix: now_unix,
            reason: reason.to_string(),
        });
        self.prune_released(now_unix);
        true
    }

    /// Drop entries whose pod is gone from the store AND whose grace
    /// elapsed (a crashed-mid-teardown or force-deleted pod cannot leak
    /// devices forever). Live pods never match: their UIDs are in
    /// `live_uids`. Returns true when the state changed.
    pub fn reconcile(&mut self, live_uids: &BTreeSet<String>, now_unix: u64) -> bool {
        let stale: Vec<String> = self
            .state
            .allocations
            .iter()
            .filter(|(uid, per_pod)| {
                !per_pod.is_empty()
                    && !live_uids.contains(*uid)
                    && now_unix.saturating_sub(per_pod.values().map(|e| e.since).min().unwrap_or(0))
                        > RECONCILE_GRACE_SECS
            })
            .map(|(uid, _)| uid.clone())
            .collect();
        let mut changed = false;
        for uid in stale {
            changed |= self.release(&uid, now_unix, "pod-absent-grace");
        }
        changed
    }

    /// Keep only the most recent [`RELEASED_RETENTION_SECS`] of records.
    pub fn prune_released(&mut self, now_unix: u64) -> bool {
        let before = self.state.released.len();
        self.state
            .released
            .retain(|r| now_unix.saturating_sub(r.at_unix) <= RELEASED_RETENTION_SECS);
        before != self.state.released.len()
    }
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod ledger_tests;
