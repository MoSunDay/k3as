//! Container resources → CRI cgroup knobs + pod QoS classification
//! (TODO **T4.9**). Pure functions only: quantities parse through
//! [`common::parse_quantity`] (the same SSOT the scheduler consumes for
//! `resources.requests`), the cpu mapping mirrors upstream kubelet's
//! `MilliCPUToShares` / `MilliCPUToQuota` conventions, and QoS is the
//! upstream `GetPodQOS` algorithm (regular containers, cpu+memory only).
//! Explicitly out (card): node capacity discovery and eviction.

use std::collections::BTreeMap;

use common::parse_quantity;
use runtime::cri_json::LinuxResources;
use serde_json::Value;

/// One container's `resources` block, quantity strings kept VERBATIM
/// (k8s wire shape; parsing happens at mapping time so equality on
/// [`crate::objects::ContainerSpec`] stays cheap and exact).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceRequirements {
    pub requests: BTreeMap<String, String>,
    pub limits: BTreeMap<String, String>,
}

/// Pod QoS class (upstream `status.qosClass` values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QoS {
    Guaranteed,
    Burstable,
    BestEffort,
}

impl QoS {
    /// The `status.qosClass` wire string.
    pub fn as_str(&self) -> &'static str {
        match self {
            QoS::Guaranteed => "Guaranteed",
            QoS::Burstable => "Burstable",
            QoS::BestEffort => "BestEffort",
        }
    }
}

/// Parse `spec.containers[].resources.{requests,limits}` (tolerant: rows
/// with empty keys/values are skipped, never fatal — malformed quantities
/// surface later as "absent" in the mapping).
pub fn parse_requirements(c: &Value) -> ResourceRequirements {
    let block = |name: &str| -> BTreeMap<String, String> {
        c.pointer(&format!("/resources/{name}"))
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| {
                        let val = v.as_str()?.trim().to_string();
                        if k.is_empty() || val.is_empty() {
                            return None;
                        }
                        Some((k.clone(), val))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    ResourceRequirements {
        requests: block("requests"),
        limits: block("limits"),
    }
}

/// Upstream `MilliCPUToShares`: 0 request → the 2-share minimum, else
/// `max(2, milli * 1024 / 1000)` (integer math, like kubelet's).
fn cpu_shares(milli: i64) -> u64 {
    if milli <= 0 {
        return 2;
    }
    ((milli * 1024) / 1000).max(2) as u64
}

/// Upstream `MilliCPUToQuota` at the default 100000 period: no limit →
/// **-1** (cap explicitly disabled, proto3 0 would mean "unset"); else
/// `milli * period / 1000`.
fn cpu_quota(milli: i64) -> i64 {
    if milli <= 0 {
        return -1;
    }
    (milli * 100_000) / 1000
}

/// The default CFS period kubelet pins whenever it sends a quota.
pub const CPU_PERIOD: u64 = 100_000;

fn cpu_milli_request(r: &ResourceRequirements) -> i64 {
    r.requests
        .get("cpu")
        .and_then(|q| parse_quantity(q))
        .map(|cores| (cores * 1000.0).round() as i64)
        .unwrap_or(0)
}

fn cpu_milli_limit(r: &ResourceRequirements) -> i64 {
    r.limits
        .get("cpu")
        .and_then(|q| parse_quantity(q))
        .map(|cores| (cores * 1000.0).round() as i64)
        .unwrap_or(0)
}

fn memory_limit_bytes(r: &ResourceRequirements) -> i64 {
    r.limits
        .get("memory")
        .and_then(|q| parse_quantity(q))
        .map(|b| b as i64)
        .unwrap_or(0)
}

/// Map one container's requirements into the CRI cgroup shape (T4.9):
/// cpu request → `cpu_shares`, cpu limit → `cpu_quota`/`cpu_period`,
/// memory limit → `memory_limit_in_bytes`. Following upstream kubelet the
/// cpu knobs are ALWAYS set (shares minimum 2, quota -1 = no cap, period
/// 100000); memory stays 0/omitted without a limit.
pub fn linux_resources(r: &ResourceRequirements) -> LinuxResources {
    LinuxResources {
        cpu_period: CPU_PERIOD,
        cpu_quota: cpu_quota(cpu_milli_limit(r)),
        cpu_shares: cpu_shares(cpu_milli_request(r)),
        memory_limit_in_bytes: memory_limit_bytes(r),
    }
}

/// Upstream `GetPodQOS` over the pod's regular containers (cpu+memory
/// only; extended resources never affect QoS):
/// - **BestEffort** — no container sets any cpu/memory request or limit;
/// - **Guaranteed** — every container sets cpu AND memory requests ==
///   limits (parsed equality, so `"100m"` == `"0.1"`);
/// - **Burstable** — everything else.
pub fn qos_class(specs: &[crate::objects::ContainerSpec]) -> QoS {
    let mut guaranteed = !specs.is_empty();
    let mut any_set = false;
    for s in specs {
        for res in ["cpu", "memory"] {
            let req = s
                .resources
                .requests
                .get(res)
                .and_then(|q| parse_quantity(q));
            let lim = s.resources.limits.get(res).and_then(|q| parse_quantity(q));
            if req.is_some() || lim.is_some() {
                any_set = true;
            }
            match (req, lim) {
                (Some(r), Some(l)) if (r - l).abs() <= f64::EPSILON => {}
                _ => guaranteed = false,
            }
        }
    }
    if !any_set {
        QoS::BestEffort
    } else if guaranteed {
        QoS::Guaranteed
    } else {
        QoS::Burstable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reqs(requests: &[(&str, &str)], limits: &[(&str, &str)]) -> ResourceRequirements {
        ResourceRequirements {
            requests: requests
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            limits: limits
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn spec(r: ResourceRequirements) -> crate::objects::ContainerSpec {
        crate::objects::ContainerSpec {
            name: "c".into(),
            image: "img:1".into(),
            command: vec![],
            args: vec![],
            startup_probe: None,
            readiness_probe: None,
            liveness_probe: None,
            volume_mounts: vec![],
            image_pull_policy: "IfNotPresent".into(),
            env: vec![],
            env_from: vec![],
            resources: r,
        }
    }

    #[test]
    fn parse_requirements_keeps_quantities_verbatim() {
        let c = json!({"resources": {
            "requests": {"cpu": "100m", "memory": "64Mi", "nvidia.com/gpu": "1"},
            "limits": {"cpu": "0.25", "memory": ""},
        }});
        let r = parse_requirements(&c);
        assert_eq!(r.requests["cpu"], "100m");
        assert_eq!(r.requests["memory"], "64Mi");
        assert_eq!(r.limits["cpu"], "0.25");
        // empty values skipped; extended resources ride along verbatim
        assert!(!r.limits.contains_key("memory"));
        assert_eq!(r.requests["nvidia.com/gpu"], "1");
        assert!(parse_requirements(&json!({})).requests.is_empty());
    }

    #[test]
    fn linux_resources_maps_upstream_conventions() {
        // 100m request + 250m limit + 64Mi memory limit
        let r = reqs(
            &[("cpu", "100m"), ("memory", "64Mi")],
            &[("cpu", "250m"), ("memory", "64Mi")],
        );
        let l = linux_resources(&r);
        assert_eq!(l.cpu_shares, 102, "100m * 1024 / 1000");
        assert_eq!(l.cpu_quota, 25_000, "250m * 100000 / 1000");
        assert_eq!(l.cpu_period, 100_000);
        assert_eq!(l.memory_limit_in_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn linux_resources_defaults_match_kubelet() {
        // No resources at all: shares floor 2, quota -1 (disabled), memory 0.
        let l = linux_resources(&ResourceRequirements::default());
        assert_eq!(l.cpu_shares, 2);
        assert_eq!(l.cpu_quota, -1);
        assert_eq!(l.cpu_period, 100_000);
        assert_eq!(l.memory_limit_in_bytes, 0);
        assert!(!l.is_empty());
        // request-only (Burstable): quota still disabled, shares from request
        let r = reqs(&[("cpu", "1")], &[]);
        let l = linux_resources(&r);
        assert_eq!(l.cpu_shares, 1024);
        assert_eq!(l.cpu_quota, -1);
        // limit without request (rare but legal): shares floor, quota set
        let r = reqs(&[], &[("cpu", "500m")]);
        let l = linux_resources(&r);
        assert_eq!(l.cpu_shares, 2);
        assert_eq!(l.cpu_quota, 50_000);
        // sub-milli rounding and Gi memory
        let r = reqs(&[("cpu", "1500m")], &[("memory", "1Gi")]);
        let l = linux_resources(&r);
        assert_eq!(l.cpu_shares, 1536);
        assert_eq!(l.memory_limit_in_bytes, 1024 * 1024 * 1024);
    }

    #[test]
    fn qos_three_classes_upstream_algorithm() {
        // BestEffort: nothing set anywhere
        assert_eq!(
            qos_class(&[spec(ResourceRequirements::default())]),
            QoS::BestEffort
        );
        // Guaranteed: cpu+memory requests == limits in every container
        let g = reqs(
            &[("cpu", "100m"), ("memory", "64Mi")],
            &[("cpu", "0.1"), ("memory", "67108864")],
        );
        assert_eq!(
            qos_class(&[spec(g)]),
            QoS::Guaranteed,
            "0.1 == 100m parsed-equal"
        );
        // Burstable: request without limit
        let b = reqs(&[("cpu", "100m")], &[]);
        assert_eq!(qos_class(&[spec(b)]), QoS::Burstable);
        // Burstable: limits set but a request mismatch
        let b2 = reqs(
            &[("cpu", "100m"), ("memory", "64Mi")],
            &[("cpu", "250m"), ("memory", "64Mi")],
        );
        assert_eq!(qos_class(&[spec(b2)]), QoS::Burstable);
        // Burstable: one Guaranteed container + one BestEffort container
        let g2 = reqs(
            &[("cpu", "1"), ("memory", "1Gi")],
            &[("cpu", "1"), ("memory", "1Gi")],
        );
        assert_eq!(
            qos_class(&[spec(g2), spec(ResourceRequirements::default())]),
            QoS::Burstable
        );
        // extended resources never affect QoS (BestEffort despite gpu limit)
        let x = reqs(&[], &[("nvidia.com/gpu", "1")]);
        assert_eq!(qos_class(&[spec(x)]), QoS::BestEffort);
        // memory-only limits: set, but not Guaranteed (cpu missing)
        let m = reqs(&[], &[("memory", "64Mi")]);
        assert_eq!(qos_class(&[spec(m)]), QoS::Burstable);
    }
}
