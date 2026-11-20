//! Resource quantity math for `ResourceFit` (T3.2): quantity parsing (in
//! `common` since T4.9, shared with the kubelet), pod request sums,
//! node allocatable, and the **logical-node default** — a Node without
//! `status.allocatable`/`capacity` is treated as unbounded (logged once per
//! node name, decision **Q23**): v1 headless/test nodes report no status.
//!
//! T7.2 (**Q61**): whole-GPU extended resources ride the SAME fit filter
//! with two deviations — resource keys are JSON-pointer-escaped
//! (`nvidia.com/gpu` -> `nvidia.com~1gpu`; see [`escape_pointer_token`],
//! fixing the GPU1 nested-key bug), and a node that does not PUBLISH a
//! GPU resource has ZERO of it (never Q23 infinity): GPU pods only ever
//! fit nodes the kubelet decorated. Fractional GPU requests are rejected
//! deterministically (whole devices only).

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::plugin::NodeInfo;

/// Quantity parsing is re-exported from `common` since T4.9 — one SSOT
/// shared with the kubelet's runtime-side mapping (the doc and tests below
/// stay as the scheduler-side contract).
pub use common::parse_quantity;

/// Escape one JSON-pointer token (RFC 6901): `~` -> `~0` FIRST, then
/// `/` -> `~1` — mandatory for extended-resource keys such as
/// `nvidia.com/gpu`, whose raw `/` would otherwise address a nested key
/// (the GPU1 bug: the request silently read as 0).
pub fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Total request of one resource across a pod's containers (+ init
/// containers — v1 approximation: upstream sums max(running, init); init
/// containers run before regular ones, so summing both is the safe bound).
pub fn pod_request(pod: &Value, resource: &str) -> f64 {
    let key = escape_pointer_token(resource);
    let containers = ["/spec/containers", "/spec/initContainers"];
    let mut total = 0.0;
    for path in containers {
        if let Some(list) = pod.pointer(path).and_then(|v| v.as_array()) {
            for c in list {
                if let Some(q) = c
                    .pointer(&format!("/resources/requests/{key}"))
                    .and_then(|v| v.as_str())
                    .and_then(parse_quantity)
                {
                    total += q;
                }
            }
        }
    }
    total
}

/// Allocatable of one resource; falls back to `status.capacity`; a node with
/// neither is a **logical node**: unbounded (Q23, logged once per name).
pub fn node_allocatable(node: &Value, resource: &str) -> f64 {
    let key = escape_pointer_token(resource);
    for path in ["/status/allocatable", "/status/capacity"] {
        if let Some(q) = node
            .pointer(&format!("{path}/{key}"))
            .and_then(|v| v.as_str())
            .and_then(parse_quantity)
        {
            return q;
        }
    }
    log_logical_node(node);
    f64::INFINITY
}

/// Sum of requests already placed on a node (assigned pods only).
pub fn node_requested(info: &NodeInfo, resource: &str) -> f64 {
    info.pods.iter().map(|p| pod_request(p, resource)).sum()
}

/// Does the pod fit the node's remaining capacity for cpu + memory?
pub fn fits(pod: &Value, info: &NodeInfo) -> bool {
    for resource in ["cpu", "memory"] {
        let alloc = node_allocatable(&info.node, resource);
        if !alloc.is_infinite()
            && pod_request(pod, resource) > alloc - node_requested(info, resource) + f64::EPSILON
        {
            return false;
        }
    }
    true
}

/// Allocatable with NO Q23 infinity: a node that does not publish the
/// resource has `0` of it (T7.2, Q61 — GPU capacity exists only where the
/// kubelet's GPU plane published it; a logical node has no GPUs).
pub fn node_allocatable_strict(node: &Value, resource: &str) -> f64 {
    let key = escape_pointer_token(resource);
    for path in ["/status/allocatable", "/status/capacity"] {
        if let Some(q) = node
            .pointer(&format!("{path}/{key}"))
            .and_then(|v| v.as_str())
            .and_then(parse_quantity)
        {
            return q;
        }
    }
    0.0
}

/// The GPU whole-device fit verdict (T7.2, Q61): `None` fits; otherwise a
/// kubectl-style rejection reason. Semantics:
/// - fractional GPU requests (e.g. `500m` of `nvidia.com/gpu`) reject
///   deterministically — whole devices only;
/// - negative requests count as 0;
/// - effective capacity = published allocatable (strict, absent = 0)
///   minus every whole-GPU request already placed on the node.
pub fn gpu_fit_reason(pod: &Value, info: &NodeInfo) -> Option<String> {
    for resource in common::gpu::GPU_RESOURCES {
        let want = pod_request(pod, resource);
        if want <= 0.0 {
            continue; // not requested (or negative -> 0)
        }
        if want.fract() != 0.0 {
            return Some(format!(
                "Insufficient {resource} (fractional GPU requests are not \
                 supported; whole devices only, Q61)"
            ));
        }
        let alloc = node_allocatable_strict(&info.node, resource);
        let used = node_requested(info, resource);
        if want > alloc - used + f64::EPSILON {
            let free = (alloc - used).max(0.0);
            return Some(format!(
                "Insufficient {resource} (requested {want}, allocatable \
                 {free} of {alloc}; whole devices only, Q61)"
            ));
        }
    }
    None
}

fn log_logical_node(node: &Value) {
    static LOGGED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let name = node
        .pointer("/metadata/name")
        .and_then(|v| v.as_str())
        .unwrap_or("<unnamed>")
        .to_string();
    let set = LOGGED.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut guard) = set.lock() {
        if guard.insert(name.clone()) {
            tracing::info!(
                target: "init-pro",
                node = %name,
                "logical node without status.allocatable: treated as unbounded (Q23)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_quantity_covers_si_binary_and_milli() {
        assert_eq!(parse_quantity("1"), Some(1.0));
        assert_eq!(parse_quantity("1.5"), Some(1.5));
        assert_eq!(parse_quantity("100m"), Some(0.1));
        assert_eq!(parse_quantity("2500m"), Some(2.5));
        assert_eq!(parse_quantity("128Mi"), Some(128.0 * 1024.0 * 1024.0));
        assert_eq!(parse_quantity("1Gi"), Some(1024.0 * 1024.0 * 1024.0));
        assert_eq!(parse_quantity("2k"), Some(2000.0));
        assert_eq!(parse_quantity("1e3"), Some(1000.0));
        assert_eq!(parse_quantity("500u"), Some(0.0005));
        assert_eq!(parse_quantity("garbage"), None);
        assert_eq!(parse_quantity(""), None);
    }

    #[test]
    fn pod_request_sums_containers_and_init() {
        let pod = json!({
            "spec": {
                "containers": [
                    {"name": "a", "resources": {"requests": {"cpu": "250m", "memory": "128Mi"}}},
                    {"name": "b", "resources": {"requests": {"cpu": "250m"}}}
                ],
                "initContainers": [
                    {"name": "i", "resources": {"requests": {"cpu": "500m", "memory": "64Mi"}}}
                ]
            }
        });
        assert!((pod_request(&pod, "cpu") - 1.0).abs() < 1e-9);
        assert!((pod_request(&pod, "memory") - 192.0 * 1024.0 * 1024.0).abs() < 1e-3);
        assert_eq!(pod_request(&pod, "nvidia.com/gpu"), 0.0);
    }

    #[test]
    fn node_without_status_is_unbounded() {
        let node = json!({"metadata": {"name": "logical"}});
        assert!(node_allocatable(&node, "cpu").is_infinite());
        let sized = json!({
            "metadata": {"name": "n"},
            "status": {"capacity": {"cpu": "4"}, "allocatable": {"cpu": "3800m", "memory": "8Gi"}}
        });
        assert!((node_allocatable(&sized, "cpu") - 3.8).abs() < 1e-9);
        assert!(node_allocatable(&sized, "memory").is_finite());
    }

    #[test]
    fn fit_respects_remaining_capacity() {
        use std::sync::Arc;
        let node = Arc::new(json!({
            "metadata": {"name": "n"},
            "status": {"allocatable": {"cpu": "1", "memory": "1Gi"}}
        }));
        let placed = Arc::new(json!({
            "metadata": {"name": "placed"},
            "spec": {"nodeName": "n", "containers": [
                {"name": "c", "resources": {"requests": {"cpu": "800m", "memory": "512Mi"}}}
            ]}
        }));
        let info = NodeInfo {
            node,
            pods: vec![placed],
        };
        let fits_small = json!({"spec": {"containers": [
            {"name": "c", "resources": {"requests": {"cpu": "100m", "memory": "256Mi"}}}
        ]}});
        let fits_no_mem = json!({"spec": {"containers": [
            {"name": "c", "resources": {"requests": {"cpu": "100m", "memory": "1Gi"}}}
        ]}});
        assert!(fits(&fits_small, &info));
        assert!(!fits(&fits_no_mem, &info));
    }

    fn gpu_node(count: &str) -> NodeInfo {
        NodeInfo {
            node: std::sync::Arc::new(json!({
                "metadata": {"name": "gpu-n"},
                "status": {"allocatable": {"cpu": "4", "nvidia.com/gpu": count}}
            })),
            pods: vec![],
        }
    }

    fn gpu_pod(count: &str) -> Value {
        json!({"spec": {"containers": [
            {"name": "c", "resources": {"requests": {"nvidia.com/gpu": count}}}
        ]}})
    }

    /// GPU1: the `/` in `nvidia.com/gpu` must round-trip through JSON
    /// pointer escaping — pre-T7.2 the raw token read a nested key and
    /// every GPU request silently counted as 0.
    #[test]
    fn gpu1_extended_resource_keys_are_pointer_escaped() {
        assert_eq!(escape_pointer_token("nvidia.com/gpu"), "nvidia.com~1gpu");
        assert_eq!(escape_pointer_token("cpu"), "cpu");
        let pod = gpu_pod("1");
        assert_eq!(pod_request(&pod, "nvidia.com/gpu"), 1.0);
        let info = gpu_node("2");
        assert_eq!(node_allocatable(&info.node, "nvidia.com/gpu"), 2.0);
        assert_eq!(node_allocatable_strict(&info.node, "nvidia.com/gpu"), 2.0);
        // Q23 regression: cpu stays unbounded on a statusless logical node.
        let logical = json!({"metadata": {"name": "logical"}});
        assert!(node_allocatable(&logical, "cpu").is_infinite());
    }

    /// GPU2: exclusive accounting — one device left of two fits one
    /// request, never two; two placed requests of a two-device node leave
    /// nothing for a third.
    #[test]
    fn gpu2_whole_devices_account_exclusively() {
        let node = std::sync::Arc::new(json!({
            "metadata": {"name": "gpu-n"},
            "status": {"allocatable": {"nvidia.com/gpu": "2"}}
        }));
        let info = NodeInfo {
            node: std::sync::Arc::clone(&node),
            pods: vec![std::sync::Arc::new(gpu_pod("1"))],
        };
        assert_eq!(gpu_fit_reason(&gpu_pod("1"), &info), None);
        assert!(gpu_fit_reason(&gpu_pod("2"), &info).is_some());
        let full = NodeInfo {
            node,
            pods: vec![
                std::sync::Arc::new(gpu_pod("1")),
                std::sync::Arc::new(gpu_pod("1")),
            ],
        };
        let reason = gpu_fit_reason(&gpu_pod("1"), &full).unwrap();
        assert!(reason.contains("Insufficient nvidia.com/gpu"), "{reason}");
    }

    /// GPU5: a node that published NO GPU capacity has zero of it (never
    /// Q23 infinity) — GPU pods are unschedulable there; fractional
    /// requests reject; negative count as 0.
    #[test]
    fn gpu5_unpublished_capacity_is_zero_and_fractions_reject() {
        let plain = NodeInfo {
            node: std::sync::Arc::new(json!({
                "metadata": {"name": "cpu-n"},
                "status": {"allocatable": {"cpu": "8"}}
            })),
            pods: vec![],
        };
        assert_eq!(node_allocatable_strict(&plain.node, "nvidia.com/gpu"), 0.0);
        let reason = gpu_fit_reason(&gpu_pod("1"), &plain).unwrap();
        assert!(reason.contains("Insufficient nvidia.com/gpu"), "{reason}");
        // Logical node (no status at all): still zero GPUs, cpu unbounded.
        let logical = NodeInfo {
            node: std::sync::Arc::new(json!({"metadata": {"name": "logical"}})),
            pods: vec![],
        };
        assert!(gpu_fit_reason(&gpu_pod("1"), &logical).is_some());
        assert!(gpu_fit_reason(&gpu_pod("0"), &plain).is_none());
        assert!(gpu_fit_reason(&gpu_pod("-1"), &plain).is_none());
        // Fractional: deterministic rejection with an actionable reason.
        let frac = gpu_fit_reason(&gpu_pod("500m"), &gpu_node("8")).unwrap();
        assert!(frac.contains("fractional"), "{frac}");
        // Integrality 1.0 passes on a big enough node.
        assert!(gpu_fit_reason(&gpu_pod("1"), &gpu_node("8")).is_none());
    }
}
