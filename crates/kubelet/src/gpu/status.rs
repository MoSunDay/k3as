//! Node GPU publication: eligibility + capacity/allocatable + the
//! `status.initProRuntime` extension (TODO **T7.2**, decision **Q61**,
//! gate **GPU5**).
//!
//! Eligibility is the Q60/Q61 triple at NODE level: the agent runs the
//! builtin engine, the engine rootfs source is `--builtin-rootfs=host`,
//! and the host-direct boot probe succeeded. A node failing any leg
//! omits BOTH GPU fields (and the extension) even if devices exist —
//! and because the heartbeat replaces `.status` wholesale, capability or
//! backend transitions zero out naturally (no stale nonzero
//! allocatable; asserted in tests below).
//!
//! The `status.initProRuntime` object is a FROZEN CONTRACT consumed by
//! the pipeline-owned canary rollout (T8.5/Q58 Recreate gate) — keep the
//! exact shape:
//!
//! ```json
//! "initProRuntime": {
//!   "gpuLedger": {
//!     "nvidia.com/gpu": {
//!       "total": 1,
//!       "allocations": {"<podUID>": {"<containerName>": [0]}}
//!     }
//!   },
//!   "released": [{"podUID": "...", "at": "<RFC3339>", "reason": "teardown-complete"}]
//! }
//! ```

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::ledger::LedgerState;
use crate::engine::RootfsMode;

/// The Node status extension key carrying the GPU ledger view (frozen
/// contract, T8.5 consumer). NOTE: contains no '/', but keep using
/// `get`-chains, never pointers, for consistency with the other
/// init-pro.io keys.
pub const INIT_PRO_RUNTIME_FIELD: &str = "initProRuntime";

/// Pure eligibility (GPU5): publish GPU inventory only when the agent
/// runs the builtin engine with a host rootfs and the host-direct boot
/// probe succeeded. `host_direct_probe_ok` is a bool input because the
/// probe already ran before the kubelet loops start (the CLI falls back
/// to containerd on probe failure, which makes `engine_builtin` false).
pub fn gpu_capable(
    engine_builtin: bool,
    engine_rootfs: RootfsMode,
    host_direct_probe_ok: bool,
) -> bool {
    engine_builtin && engine_rootfs == RootfsMode::Host && host_direct_probe_ok
}

/// Decorate one Node `.status` object in place with the GPU fields:
/// `capacity[resource] = total`, `allocatable[resource] = total -
/// allocated-by-ledger` (both as string quantities, k8s wire shape), plus
/// the [`INIT_PRO_RUNTIME_FIELD`] extension. An incapable node gets NO
/// GPU keys at all — including removal of any previously-published ones
/// is unnecessary because callers rebuild the status wholesale; this
/// function simply never writes them (asserted in tests).
pub fn decorate_node_status(
    status: &mut Value,
    totals: &BTreeMap<String, u64>,
    capable: bool,
    ledger: &LedgerState,
) {
    if !capable {
        return;
    }
    let mut capacity = status.get("capacity").cloned().unwrap_or_else(|| json!({}));
    let mut allocatable = status
        .get("allocatable")
        .cloned()
        .unwrap_or_else(|| json!({}));
    for (resource, total) in totals {
        let allocated = ledger.allocated_count(resource);
        let free = total.saturating_sub(allocated);
        capacity[resource.as_str()] = Value::String(total.to_string());
        allocatable[resource.as_str()] = Value::String(free.to_string());
    }
    status["capacity"] = capacity;
    status["allocatable"] = allocatable;
    if !totals.is_empty() {
        status[INIT_PRO_RUNTIME_FIELD] = init_pro_runtime(totals, ledger);
    }
}

/// Build the frozen `initProRuntime` value (pure). `total` per resource
/// plus the live allocation view keyed `podUID -> container -> [idx]`.
pub fn init_pro_runtime(totals: &BTreeMap<String, u64>, ledger: &LedgerState) -> Value {
    let mut gpu_ledger = json!({});
    for (resource, total) in totals {
        let mut allocations = json!({});
        for (uid, per_pod) in &ledger.allocations {
            let mut containers = json!({});
            for (container, entry) in per_pod {
                if entry.resource != *resource {
                    continue;
                }
                containers[container.as_str()] = json!(entry.devices);
            }
            if containers.as_object().is_some_and(|m| !m.is_empty()) {
                allocations[uid.as_str()] = containers;
            }
        }
        gpu_ledger[resource.as_str()] = json!({
            "total": total,
            "allocations": allocations,
        });
    }
    json!({
        "gpuLedger": gpu_ledger,
        "released": ledger
            .released
            .iter()
            .map(|r| json!({
                "podUID": r.pod_uid,
                "at": r.at,
                "reason": r.reason,
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::ledger::{AllocationEntry, GpuLedger, Want, LEDGER_VERSION};

    fn base_status() -> Value {
        json!({
            "capacity": {"cpu": "4", "memory": "8Gi", "pods": "110"},
            "allocatable": {"cpu": "4", "memory": "8Gi", "pods": "110"},
        })
    }

    fn ledger_with(uid: &str, container: &str, devices: &[u64]) -> LedgerState {
        let mut state = LedgerState {
            version: LEDGER_VERSION,
            ..Default::default()
        };
        state.allocations.insert(
            uid.to_string(),
            BTreeMap::from([(
                container.to_string(),
                AllocationEntry {
                    resource: "nvidia.com/gpu".to_string(),
                    devices: devices.to_vec(),
                    since: 100,
                },
            )]),
        );
        state
    }

    #[test]
    fn gpu5_eligibility_triple() {
        assert!(gpu_capable(true, RootfsMode::Host, true));
        // Every broken leg omits publication.
        assert!(!gpu_capable(false, RootfsMode::Host, true));
        assert!(!gpu_capable(true, RootfsMode::Image, true));
        assert!(!gpu_capable(true, RootfsMode::Host, false));
    }

    #[test]
    fn gpu5_incapable_node_omits_both_fields_entirely() {
        let mut s = base_status();
        let totals = BTreeMap::from([("nvidia.com/gpu".to_string(), 2u64)]);
        decorate_node_status(&mut s, &totals, false, &ledger_with("u", "c", &[0]));
        assert!(s["capacity"].get("nvidia.com/gpu").is_none());
        assert!(s["allocatable"].get("nvidia.com/gpu").is_none());
        assert!(s.get(INIT_PRO_RUNTIME_FIELD).is_none());
    }

    #[test]
    fn gpu5_capability_transition_zeroes_fields_via_wholesale_rebuild() {
        // Capable -> published; then the SAME constructor path without
        // capability rebuilds the status and the fields are gone (no
        // stale nonzero allocatable).
        let totals = BTreeMap::from([("nvidia.com/gpu".to_string(), 1u64)]);
        let mut capable_status = base_status();
        decorate_node_status(&mut capable_status, &totals, true, &LedgerState::default());
        assert_eq!(capable_status["capacity"]["nvidia.com/gpu"], "1");
        assert_eq!(capable_status["allocatable"]["nvidia.com/gpu"], "1");
        // Heartbeat semantics: fresh base status + incapable decorate.
        let mut rebuilt = base_status();
        decorate_node_status(&mut rebuilt, &totals, false, &LedgerState::default());
        assert!(rebuilt["capacity"].get("nvidia.com/gpu").is_none());
        assert!(rebuilt["allocatable"].get("nvidia.com/gpu").is_none());
    }

    #[test]
    fn publication_allocatable_is_total_minus_ledger() {
        let mut s = base_status();
        let totals = BTreeMap::from([("nvidia.com/gpu".to_string(), 4u64)]);
        decorate_node_status(&mut s, &totals, true, &ledger_with("u", "c", &[1, 3]));
        assert_eq!(s["capacity"]["nvidia.com/gpu"], "4");
        assert_eq!(s["allocatable"]["nvidia.com/gpu"], "2");
        // Non-GPU fields untouched.
        assert_eq!(s["capacity"]["cpu"], "4");
        assert_eq!(s["allocatable"]["pods"], "110");
    }

    #[test]
    fn init_pro_runtime_contract_shape() {
        let totals = BTreeMap::from([("nvidia.com/gpu".to_string(), 1u64)]);
        let mut ledger = ledger_with("uid-1", "llama", &[0]);
        ledger.released = vec![crate::gpu::ledger::ReleasedRecord {
            pod_uid: "uid-0".into(),
            at: "2026-11-19T12:00:00Z".into(),
            at_unix: 1_763_822_400,
            reason: "teardown-complete".into(),
        }];
        let v = init_pro_runtime(&totals, &ledger);
        assert_eq!(
            v,
            json!({
                "gpuLedger": {
                    "nvidia.com/gpu": {
                        "total": 1,
                        "allocations": {"uid-1": {"llama": [0]}}
                    }
                },
                "released": [
                    {"podUID": "uid-0", "at": "2026-11-19T12:00:00Z", "reason": "teardown-complete"}
                ]
            })
        );
        // And it lands under the exact status key.
        let mut s = base_status();
        decorate_node_status(&mut s, &totals, true, &ledger);
        assert_eq!(s[INIT_PRO_RUNTIME_FIELD], v);
    }

    #[test]
    fn empty_totals_publish_no_extension() {
        let mut s = base_status();
        decorate_node_status(&mut s, &BTreeMap::new(), true, &LedgerState::default());
        assert!(s.get(INIT_PRO_RUNTIME_FIELD).is_none());
    }

    #[test]
    fn ledger_types_are_plain_data() {
        // Compile-shape guard: the ledger state round-trips through the
        // exact file format (plain data, serde-owned).
        let mut l = GpuLedger::open("/tmp/definitely/not/used");
        let wants: Vec<Want> = vec![("c".to_string(), "amd.com/gpu".to_string(), 2)];
        let ok = l.allocate_pod(
            "u",
            &wants,
            &BTreeMap::from([("amd.com/gpu".to_string(), 2)]),
            5,
        );
        assert!(ok.is_ok());
        assert_eq!(l.state.version, LEDGER_VERSION);
    }
}
