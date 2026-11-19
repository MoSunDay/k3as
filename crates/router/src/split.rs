//! `TrafficSplit` weighting plan for the NodePort plane (TODO **T8.2**,
//! decision **Q57**).
//!
//! A split applies to the **NodePort entry Service**: `spec.backends[]`
//! name backend Services with relative weights. The plan is pure:
//!
//! 1. drop `weight <= 0` backends (openrusty rejects zero-weight peers);
//! 2. resolve each backend's ready peers (same port as the entry Service,
//!    with a single-port fallback for drifted backend Services);
//! 3. divide the backend weight across its endpoints, scaled by 100, so the
//!    traffic ratio is replica-count independent (`90/10` stays 90/10 with
//!    2 stable pods and 1 canary pod);
//! 4. emit a deterministic (sorted, deduped) peer list — weight-only edits
//!    reload the openrusty config, identical edits do not.
//!
//! No header/cookie pinning in v1 (needs openrusty header match — future).

use std::net::SocketAddr;

use crate::endpoints::{resolve_backend, ResolverState};
use crate::route::UpstreamRef;

/// One openrusty peer: address + relative weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerWeight {
    pub addr: SocketAddr,
    pub weight: u32,
}

/// The folded, Router-side view of a `TrafficSplit`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SplitView {
    pub namespace: String,
    pub name: String,
    /// Entry Service this split controls.
    pub service: String,
    pub backends: Vec<SplitBackendView>,
}

/// One backend Service + relative weight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitBackendView {
    pub service: String,
    pub weight: i32,
}

impl SplitView {
    /// Build the view from the API type (keyed `ns/name` by the reflector).
    pub fn from_api(value: &api::traffic::TrafficSplit) -> Option<Self> {
        let name = value.metadata.name.clone()?;
        if name.is_empty() || value.spec.service.trim().is_empty() {
            return None;
        }
        let namespace = value
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        Some(Self {
            namespace,
            name,
            service: value.spec.service.clone(),
            backends: value
                .spec
                .backends
                .iter()
                .map(|b| SplitBackendView {
                    service: b.service.clone(),
                    weight: b.weight,
                })
                .collect(),
        })
    }
}

/// The resolved target set for one listener.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Backends the gate must consider (entry Service when no split).
    pub targets: Vec<UpstreamRef>,
    /// Weighted peers for the openrusty upstream.
    pub peers: Vec<PeerWeight>,
    /// `true` when a split governs this entry Service.
    pub split: bool,
}

/// Split the entry key into `(namespace?, name)` (`default`-less bare keys
/// keep an empty namespace).
fn ns_name(service: &str) -> (&str, &str) {
    match service.split_once('/') {
        Some((ns, name)) => (ns, name),
        None => ("", service),
    }
}

/// Find the split governing `service` (`ns/name` exact, else bare name).
pub fn find_split<'a>(
    state: &'a ResolverState,
    service: &str,
) -> Option<(&'a String, &'a SplitView)> {
    if let Some(kv) = state.splits.get_key_value(service) {
        return Some(kv);
    }
    let (ns, name) = ns_name(service);
    state.splits.iter().find(|(_, v)| {
        v.name == name
            && (ns.is_empty() || v.namespace == ns)
            && v.service.split('/').next_back() == Some(name)
    })
}

/// Resolve the listener plan for `entry` against a live state snapshot.
pub fn plan_for(state: &ResolverState, entry: &UpstreamRef) -> Plan {
    let Some((_, split)) = find_split(state, &entry.service) else {
        return Plan {
            targets: vec![entry.clone()],
            peers: uniform(resolve_backend(state, entry)),
            split: false,
        };
    };
    let active: Vec<&SplitBackendView> = split.backends.iter().filter(|b| b.weight > 0).collect();
    let targets: Vec<UpstreamRef> = active
        .iter()
        .map(|b| UpstreamRef {
            service: qualify(&split.namespace, &b.service),
            port: entry.port.clone(),
        })
        .collect();
    let mut peers = Vec::new();
    for (backend, target) in active.iter().zip(&targets) {
        let mut addrs = resolve_backend(state, target);
        addrs.sort();
        addrs.dedup();
        for (addr, weight) in addrs
            .iter()
            .zip(endpoint_weights(backend.weight, addrs.len()))
        {
            peers.push(PeerWeight {
                addr: *addr,
                weight,
            });
        }
    }
    // Deterministic order for reload comparison (weights stay attached).
    peers.sort_by_key(|a| a.addr);
    peers.dedup_by(|a, b| a.addr == b.addr && a.weight == b.weight);
    Plan {
        targets,
        peers,
        split: true,
    }
}

/// Qualify a backend Service name with the split's namespace when bare.
fn qualify(namespace: &str, service: &str) -> String {
    if service.contains('/') {
        service.to_string()
    } else {
        format!("{namespace}/{service}")
    }
}

/// Uniform weight-1 peers (no split: plain Endpoints round-robin).
pub fn uniform(addrs: Vec<SocketAddr>) -> Vec<PeerWeight> {
    let mut out: Vec<PeerWeight> = addrs
        .into_iter()
        .map(|addr| PeerWeight { addr, weight: 1 })
        .collect();
    out.sort_by_key(|a| a.addr);
    out.dedup_by(|a, b| a.addr == b.addr);
    out
}

/// Divide `weight` across `n` endpoints, scaled by 100 (Q57). The first
/// `basis % n` endpoints get the remainder so the sum is exact; a zero base
/// floors at 1 (openrusty rejects zero weights).
pub fn endpoint_weights(weight: i32, n: usize) -> Vec<u32> {
    if n == 0 || weight <= 0 {
        return Vec::new();
    }
    let basis = (weight as u64) * 100;
    let base = (basis / n as u64) as u32;
    let rem = (basis % n as u64) as usize;
    (0..n).map(|i| (base + u32::from(i < rem)).max(1)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::{EndpointsView, EpPort, EpSubset, ServiceView, SvcPort, TargetPort};

    fn svc(name: &str, port: u16) -> ServiceView {
        ServiceView {
            namespace: "default".into(),
            name: name.into(),
            kind_type: "ClusterIP".into(),
            ports: vec![SvcPort {
                name: None,
                protocol: Some("TCP".into()),
                port,
                target_port: Some(TargetPort::Number(port + 8000)),
                node_port: None,
            }],
            ingress_ip: None,
        }
    }

    fn ep(ips: &[&str], port: u16) -> EndpointsView {
        EndpointsView {
            subsets: vec![EpSubset {
                addresses: ips.iter().map(|s| s.to_string()).collect(),
                ports: vec![EpPort { name: None, port }],
            }],
        }
    }

    fn state() -> ResolverState {
        let mut st = ResolverState::default();
        st.services.insert("default/entry".into(), svc("entry", 80));
        st.services
            .insert("default/stable".into(), svc("stable", 80));
        st.services
            .insert("default/canary".into(), svc("canary", 80));
        st.endpoints
            .insert("default/stable".into(), ep(&["10.0.0.1", "10.0.0.2"], 8080));
        st.endpoints
            .insert("default/canary".into(), ep(&["10.0.0.3"], 8080));
        st.splits.insert(
            "default/entry".into(),
            SplitView {
                namespace: "default".into(),
                name: "entry".into(),
                service: "entry".into(),
                backends: vec![
                    SplitBackendView {
                        service: "stable".into(),
                        weight: 90,
                    },
                    SplitBackendView {
                        service: "canary".into(),
                        weight: 10,
                    },
                ],
            },
        );
        st
    }

    fn peer(s: &str) -> SocketAddr {
        SocketAddr::new(s.parse().expect("literal ip"), 8080)
    }

    #[test]
    fn no_split_is_uniform_over_entry_endpoints() {
        let mut st = state();
        st.splits.clear();
        st.endpoints
            .insert("default/entry".into(), ep(&["10.0.0.9", "10.0.0.8"], 8080));
        let plan = plan_for(&st, &UpstreamRef::port("default/entry", 80));
        assert!(!plan.split);
        assert_eq!(plan.targets, vec![UpstreamRef::port("default/entry", 80)]);
        assert_eq!(plan.peers.len(), 2);
        assert!(plan.peers.iter().all(|p| p.weight == 1));
        // deterministic order
        assert!(plan.peers[0].addr < plan.peers[1].addr);
    }

    #[test]
    fn split_weights_are_replica_count_independent() {
        let plan = plan_for(&state(), &UpstreamRef::port("default/entry", 80));
        assert!(plan.split);
        assert_eq!(plan.targets.len(), 2);
        // stable: 2 pods x 45, canary: 1 pod x 10 -> ratio 90:10
        assert_eq!(plan.peers.len(), 3);
        let total: u32 = plan.peers.iter().map(|p| p.weight).sum();
        assert_eq!(total, 10_000); // weights are scaled by 100 (Q57)
        let stable: u32 = plan
            .peers
            .iter()
            .filter(|p| p.addr != peer("10.0.0.3"))
            .map(|p| p.weight)
            .sum();
        assert_eq!(stable, 9_000);
    }

    #[test]
    fn zero_weight_backend_is_dropped() {
        let mut st = state();
        if let Some(s) = st.splits.get_mut("default/entry") {
            s.backends[1].weight = 0;
        }
        let plan = plan_for(&st, &UpstreamRef::port("default/entry", 80));
        assert_eq!(plan.targets, vec![UpstreamRef::port("default/stable", 80)]);
        assert!(plan.peers.iter().all(|p| p.addr != peer("10.0.0.3")));
        assert_eq!(plan.peers.iter().map(|p| p.weight).sum::<u32>(), 9_000);
    }

    #[test]
    fn all_zero_backends_yield_empty_plan() {
        let mut st = state();
        if let Some(s) = st.splits.get_mut("default/entry") {
            for b in &mut s.backends {
                b.weight = 0;
            }
        }
        let plan = plan_for(&st, &UpstreamRef::port("default/entry", 80));
        assert!(plan.split);
        assert!(plan.targets.is_empty());
        assert!(plan.peers.is_empty());
    }

    #[test]
    fn missing_backend_service_is_skipped() {
        let mut st = state();
        st.endpoints.remove("default/canary");
        let plan = plan_for(&st, &UpstreamRef::port("default/entry", 80));
        assert_eq!(plan.targets.len(), 2);
        assert_eq!(plan.peers.len(), 2);
    }

    #[test]
    fn single_port_fallback_resolves_backend_on_another_port() {
        let mut st = state();
        let mut canary = svc("canary", 9090);
        canary.ports[0].target_port = Some(TargetPort::Number(9091));
        st.services.insert("default/canary".into(), canary);
        // Endpoints publish the container port for the single-port Service.
        st.endpoints
            .insert("default/canary".into(), ep(&["10.0.0.3"], 9091));
        let plan = plan_for(&st, &UpstreamRef::port("default/entry", 80));
        assert!(plan.peers.iter().any(|p| p.addr.port() == 9091));
    }

    #[test]
    fn endpoint_weights_split_exactly() {
        assert_eq!(endpoint_weights(90, 2), vec![4500, 4500]);
        assert_eq!(endpoint_weights(10, 1), vec![1000]);
        assert_eq!(endpoint_weights(5, 3), vec![167, 167, 166]);
        assert_eq!(endpoint_weights(1, 1000), vec![1u32; 1000]);
        assert!(endpoint_weights(0, 3).is_empty());
        assert!(endpoint_weights(10, 0).is_empty());
    }

    #[test]
    fn from_api_requires_service_and_name() {
        let mut ts = api::traffic::new_traffic_split(
            "entry",
            "default",
            api::traffic::TrafficSplitSpec {
                service: "entry".into(),
                backends: vec![api::traffic::SplitBackend {
                    service: "stable".into(),
                    weight: 1,
                }],
            },
        );
        assert_eq!(SplitView::from_api(&ts).unwrap().backends.len(), 1);
        ts.spec.service = String::new();
        assert!(SplitView::from_api(&ts).is_none());
    }
}
