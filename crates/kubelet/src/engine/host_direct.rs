//! Host-direct selection contract (T4.15, Q60).
//!
//! Pure, side-effect-free decision functions shared by the runtime
//! (builtin engine + nsrun child setup), the kubelet sync guards (a
//! host-direct or GPU request must NEVER execute through containerd),
//! and tests. Q60 requires the strict triple before any image planning:
//!
//! 1. the agent runs `--container-engine=builtin`;
//! 2. the engine-level rootfs source is `--builtin-rootfs=host`;
//! 3. the Pod carries the LITERAL annotation `init-pro.io/rootfs: /`.
//!
//! `image` stays declarative wire metadata on this path — literal `/`,
//! not `image`, selects the host rootfs. A literal `/` annotation with
//! an incomplete triple is an INVALID selection: the pod fails with an
//! actionable reason and no fallback (Q60 forbids containerd fallback,
//! silent or explicit).

use serde_json::Value;
use std::collections::BTreeMap;

pub use super::rootfs::{RootfsMode, ROOTFS_ANNOTATION};

/// The one literal annotation value that selects host-direct (Q60).
pub const HOST_DIRECT_ROOT: &str = "/";

/// Outcome of evaluating the Q60 strict triple for one pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostDirectSelection {
    /// Triple complete: run with the host root, no image actions.
    Selected,
    /// Annotation absent or a normal `host`/`image` value: the Q55/Q53
    /// rootfs path applies unchanged (not host-direct).
    NotSelected,
    /// Annotation is literal `/` but the triple is broken — fail the
    /// pod with the carried reason; never fall back to another path.
    Invalid(String),
}

/// Evaluate the strict triple. `engine_builtin` is whether this agent
/// runs `--container-engine=builtin`; `engine_rootfs` is the
/// `--builtin-rootfs` flag value.
pub fn select_host_direct(
    engine_builtin: bool,
    engine_rootfs: RootfsMode,
    annotation: Option<&str>,
) -> HostDirectSelection {
    let Some(value) = annotation else {
        return HostDirectSelection::NotSelected;
    };
    if value != HOST_DIRECT_ROOT {
        // `host`/`image` keep Q55 semantics; anything else is invalid
        // rootfs-annotation syntax handled by resolve_mode validation.
        return HostDirectSelection::NotSelected;
    }
    if !engine_builtin {
        return HostDirectSelection::Invalid(format!(
            "annotation {ROOTFS_ANNOTATION}: {HOST_DIRECT_ROOT} requires --container-engine=builtin \
             (host-direct never runs through containerd; Q60 forbids fallback)"
        ));
    }
    if engine_rootfs != RootfsMode::Host {
        return HostDirectSelection::Invalid(format!(
            "annotation {ROOTFS_ANNOTATION}: {HOST_DIRECT_ROOT} requires --builtin-rootfs=host \
             (Q60 strict triple; refusing to reinterpret literal / under image mode)"
        ));
    }
    HostDirectSelection::Selected
}

/// True when the pod's `init-pro.io/rootfs` annotation is literally `/`
/// (the host-direct REQUEST, independent of node capability).
pub fn host_direct_requested(pod: &Value) -> bool {
    pod.pointer("/metadata/annotations")
        .and_then(Value::as_object)
        .and_then(|m| m.get(ROOTFS_ANNOTATION))
        .and_then(Value::as_str)
        .map(|v| v == HOST_DIRECT_ROOT)
        .unwrap_or(false)
}

/// Sum of whole-GPU extended-resource requests across all containers
/// (T7.2, Q61): vendor resource -> requested device count. GPU pods are
/// ordinary-inference workloads and must ride host-direct; containerd
/// execution is forbidden for them exactly as for `/` requests.
pub fn pod_gpu_requests(pod: &Value) -> BTreeMap<String, i64> {
    let mut out: BTreeMap<String, i64> = BTreeMap::new();
    let mut containers: Vec<&Value> = Vec::new();
    for path in ["/spec/containers", "/spec/initContainers"] {
        if let Some(list) = pod.pointer(path).and_then(Value::as_array) {
            containers.extend(list.iter());
        }
    }
    for c in containers {
        let Some(reqs) = c.pointer("/resources/requests").and_then(Value::as_object) else {
            continue;
        };
        for (res, qty) in reqs {
            if !common::gpu::is_gpu_resource(res) {
                continue;
            }
            let n = match qty {
                Value::Number(n) => n.as_f64().unwrap_or(0.0),
                Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
                _ => 0.0,
            };
            if n > 0.0 {
                *out.entry(res.to_string()).or_insert(0) += n as i64;
            }
        }
    }
    out
}

/// True when the pod requests at least one whole GPU (Q61).
pub fn pod_uses_gpu(pod: &Value) -> bool {
    !pod_gpu_requests(pod).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ann(v: &str) -> Option<&str> {
        Some(v)
    }

    #[test]
    fn hd1_triple_positive() {
        assert_eq!(
            select_host_direct(true, RootfsMode::Host, ann("/")),
            HostDirectSelection::Selected
        );
    }

    #[test]
    fn hd1_missing_annotation_or_nonliteral() {
        assert_eq!(
            select_host_direct(true, RootfsMode::Host, None),
            HostDirectSelection::NotSelected
        );
        assert_eq!(
            select_host_direct(true, RootfsMode::Host, ann("host")),
            HostDirectSelection::NotSelected
        );
        assert_eq!(
            select_host_direct(true, RootfsMode::Host, ann("image")),
            HostDirectSelection::NotSelected
        );
        // nonliteral near-misses must NOT select host-direct
        for v in ["/ ", "./", "/bin", ""] {
            assert_eq!(
                select_host_direct(true, RootfsMode::Host, Some(v)),
                HostDirectSelection::NotSelected,
                "value {v:?}"
            );
        }
    }

    #[test]
    fn hd1_broken_triple_is_invalid_never_fallback() {
        assert!(matches!(
            select_host_direct(false, RootfsMode::Host, ann("/")),
            HostDirectSelection::Invalid(_)
        ));
        assert!(matches!(
            select_host_direct(true, RootfsMode::Image, ann("/")),
            HostDirectSelection::Invalid(_)
        ));
    }

    #[test]
    fn pod_predicates() {
        let pod = json!({
            "metadata": {"annotations": {ROOTFS_ANNOTATION: "/"}},
            "spec": {"containers": [{"resources": {"requests": {
                "nvidia.com/gpu": "1", "cpu": "2"
            }}}]}
        });
        assert!(host_direct_requested(&pod));
        assert!(pod_uses_gpu(&pod));
        assert_eq!(
            pod_gpu_requests(&pod)
                .get("nvidia.com/gpu")
                .copied()
                .unwrap_or(0),
            1
        );
        let plain = json!({"metadata": {}, "spec": {"containers": [{}]}});
        assert!(!host_direct_requested(&plain));
        assert!(!pod_uses_gpu(&plain));
    }
}
