//! Resource quantity math for `ResourceFit` (T3.2): Kubernetes quantity
//! parsing (decimal SI + binary SI + milli/micro/nano), pod request sums,
//! node allocatable, and the **logical-node default** — a Node without
//! `status.allocatable`/`capacity` is treated as unbounded (logged once per
//! node name, decision **Q23**): v1 headless/test nodes report no status.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::plugin::NodeInfo;

/// Parse one Kubernetes quantity (`"100m"`, `"1.5"`, `"128Mi"`, `"1e3"`,
/// `"2Ki"`) into base units (cores for cpu, bytes for memory). Returns `None`
/// for malformed values; a missing field is handled by callers, not here.
pub fn parse_quantity(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Find the split between mantissa and suffix (last char boundary where
    // the remainder is a known unit).
    const SUFFIXES: &[(&str, f64)] = &[
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
        ("Ki", 1024.0),
        ("Mi", 1_048_576.0),
        ("Gi", 1_073_741_824.0),
        ("Ti", 1_099_511_627_776.0),
        ("Pi", 1_125_899_906_842_624.0),
        ("Ei", 1_152_921_504_606_846_976.0),
    ];
    for (suffix, mult) in SUFFIXES {
        if let Some(num) = s.strip_suffix(suffix) {
            let v: f64 = num.parse().ok()?;
            return Some(v * mult);
        }
    }
    s.parse::<f64>().ok()
}

/// Total request of one resource across a pod's containers (+ init
/// containers — v1 approximation: upstream sums max(running, init); init
/// containers run before regular ones, so summing both is the safe bound).
pub fn pod_request(pod: &Value, resource: &str) -> f64 {
    let containers = ["/spec/containers", "/spec/initContainers"];
    let mut total = 0.0;
    for path in containers {
        if let Some(list) = pod.pointer(path).and_then(|v| v.as_array()) {
            for c in list {
                if let Some(q) = c
                    .pointer(&format!("/resources/requests/{resource}"))
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
    for path in ["/status/allocatable", "/status/capacity"] {
        if let Some(q) = node
            .pointer(&format!("{path}/{resource}"))
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
}
