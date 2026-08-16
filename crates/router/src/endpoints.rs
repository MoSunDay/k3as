//! Live Endpoints-backed upstream resolution (Sprint 18 / **S4**, Q28 —
//! recorded S7): views + lenient parsing + state fold + peer resolution for
//! the NodePort service plane, fed by the [`crate::endpoints_watch`]
//! reflector. Empty/missing Endpoints -> zero peers -> proxy answers 503.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use serde_json::Value;
use storage::{StoredEntry, WatchEvent};
use tokio::sync::watch;

use crate::balancer::UpstreamResolver;
use crate::route::{PortRef, UpstreamRef};

/// A Service port view (`spec.ports[]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvcPort {
    pub name: Option<String>,
    pub protocol: Option<String>,
    /// The Service port number.
    pub port: u16,
    /// `targetPort`; absent = identity (the Service port number).
    pub target_port: Option<TargetPort>,
    /// Allocated nodePort (Sprint 18 / S3) for NodePort-type Services.
    pub node_port: Option<u16>,
}

/// A `targetPort`: numeric or a port name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetPort {
    Number(u16),
    Named(String),
}

/// A Service view, keyed `ns/name` in [`ResolverState`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceView {
    pub namespace: String,
    pub name: String,
    /// `spec.type` (defaulted `ClusterIP` when absent).
    pub kind_type: String,
    pub ports: Vec<SvcPort>,
}

/// Endpoints port entry; `port` is the resolved container port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpPort {
    pub name: Option<String>,
    pub port: u16,
}

/// One Endpoints subset (`notReadyAddresses` are skipped: not serving).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EpSubset {
    pub addresses: Vec<String>,
    pub ports: Vec<EpPort>,
}

/// An Endpoints view, keyed `ns/name` in [`ResolverState`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EndpointsView {
    pub subsets: Vec<EpSubset>,
}

/// The folded cluster view: Services + Endpoints keyed `ns/name`.
#[derive(Clone, Debug, Default)]
pub struct ResolverState {
    pub services: BTreeMap<String, ServiceView>,
    pub endpoints: BTreeMap<String, EndpointsView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resource {
    Services,
    Endpoints,
}

fn as_u16(v: &Value) -> Option<u16> {
    v.as_u64().and_then(|n| u16::try_from(n).ok())
}

fn nonempty_str(v: &Value, key: &str) -> Option<String> {
    let s = v.get(key).and_then(Value::as_str)?;
    (!s.is_empty()).then(|| s.to_string())
}

fn parse_target_port(v: &Value) -> Option<TargetPort> {
    match v {
        Value::Number(_) => Some(TargetPort::Number(as_u16(v)?)),
        Value::String(s) if !s.is_empty() => match s.parse::<u16>() {
            Ok(n) => Some(TargetPort::Number(n)),
            Err(_) => Some(TargetPort::Named(s.clone())),
        },
        _ => None,
    }
}

fn parse_svc_port(p: &Value) -> Option<SvcPort> {
    Some(SvcPort {
        name: nonempty_str(p, "name"),
        protocol: nonempty_str(p, "protocol"),
        port: as_u16(p.get("port")?)?,
        target_port: p.get("targetPort").and_then(parse_target_port),
        node_port: p.get("nodePort").and_then(as_u16),
    })
}

/// Parse a stored Service document; `None` = malformed (skip it).
pub fn parse_service(value: &Value) -> Option<ServiceView> {
    let meta = value.get("metadata")?;
    let name = nonempty_str(meta, "name")?;
    let namespace = nonempty_str(meta, "namespace").unwrap_or_else(|| "default".into());
    let spec = value.get("spec")?;
    let kind_type = nonempty_str(spec, "type").unwrap_or_else(|| "ClusterIP".into());
    let ports = spec
        .get("ports")
        .and_then(Value::as_array)
        .map(|ps| ps.iter().filter_map(parse_svc_port).collect())
        .unwrap_or_default();
    Some(ServiceView {
        namespace,
        name,
        kind_type,
        ports,
    })
}

fn parse_ep_subset(s: &Value) -> EpSubset {
    let arr = |k: &str| s.get(k).and_then(Value::as_array).into_iter().flatten();
    EpSubset {
        // `notReadyAddresses` intentionally skipped.
        addresses: arr("addresses")
            .filter_map(|a| nonempty_str(a, "ip"))
            .collect(),
        ports: arr("ports")
            .filter_map(|p| {
                Some(EpPort {
                    name: nonempty_str(p, "name"),
                    port: as_u16(p.get("port")?)?,
                })
            })
            .collect(),
    }
}

/// Parse a stored Endpoints document; `None` = malformed (skip it).
pub fn parse_endpoints(value: &Value) -> Option<EndpointsView> {
    nonempty_str(value.get("metadata")?, "name")?;
    let subsets = value
        .get("subsets")
        .and_then(Value::as_array)
        .map(|ss| ss.iter().map(parse_ep_subset).collect())
        .unwrap_or_default();
    Some(EndpointsView { subsets })
}

pub(crate) fn ns_name_of(value: &Value) -> Option<String> {
    let meta = value.get("metadata")?;
    let name = nonempty_str(meta, "name")?;
    let ns = nonempty_str(meta, "namespace").unwrap_or_else(|| "default".into());
    Some(format!("{ns}/{name}"))
}

/// `ns/name` from a `/registry/<resource>/<ns>/<name>` storage path (delete
/// events without a `prev` payload): last two segments, informer-style.
pub(crate) fn ns_name_from_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/registry/").unwrap_or(path);
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match segs.len() {
        0 | 1 => None,
        n => Some(format!("{}/{}", segs[n - 2], segs[n - 1])),
    }
}

/// Replace one resource's map wholesale (the re-LIST step).
pub(crate) fn replace_all(state: &mut ResolverState, resource: Resource, entries: &[StoredEntry]) {
    match resource {
        Resource::Services => {
            state.services = entries
                .iter()
                .filter_map(|e| Some((ns_name_of(&e.value)?, parse_service(&e.value)?)))
                .collect();
        }
        Resource::Endpoints => {
            state.endpoints = entries
                .iter()
                .filter_map(|e| Some((ns_name_of(&e.value)?, parse_endpoints(&e.value)?)))
                .collect();
        }
    }
}

fn insert_or_drop<V>(map: &mut BTreeMap<String, V>, key: String, view: Option<V>) {
    match view {
        Some(v) => {
            map.insert(key, v);
        }
        None => {
            map.remove(&key);
        }
    }
}

/// Apply one watch event (idempotent; a malformed Put drops the key rather
/// than poisoning it).
pub(crate) fn fold_event(state: &mut ResolverState, resource: Resource, ev: &WatchEvent) {
    match ev {
        WatchEvent::Put(e) => {
            let Some(key) = ns_name_of(&e.value).or_else(|| ns_name_from_path(&e.key)) else {
                return;
            };
            match resource {
                Resource::Services => {
                    insert_or_drop(&mut state.services, key, parse_service(&e.value));
                }
                Resource::Endpoints => {
                    insert_or_drop(&mut state.endpoints, key, parse_endpoints(&e.value));
                }
            }
        }
        WatchEvent::Delete { key, prev, .. } => {
            let k = prev
                .as_ref()
                .and_then(|p| ns_name_of(&p.value))
                .or_else(|| ns_name_from_path(key));
            if let Some(k) = k {
                match resource {
                    Resource::Services => drop(state.services.remove(&k)),
                    Resource::Endpoints => drop(state.endpoints.remove(&k)),
                }
            }
        }
    }
}

/// Lookup: exact `ns/name`, else bare name (first `BTreeMap` match).
fn find_service<'a>(
    state: &'a ResolverState,
    service: &str,
) -> Option<(&'a String, &'a ServiceView)> {
    if state.services.contains_key(service) {
        return state.services.get_key_value(service);
    }
    let bare = service.rsplit('/').next().unwrap_or(service);
    state.services.iter().find(|(_, v)| v.name == bare)
}

/// Resolve one upstream against a state snapshot (pure; unit-testable).
pub fn resolve_with_state(state: &ResolverState, upstream: &UpstreamRef) -> Vec<SocketAddr> {
    let Some((key, svc)) = find_service(state, &upstream.service) else {
        return Vec::new();
    };
    let svc_port = match &upstream.port {
        PortRef::Number(n) => svc.ports.iter().find(|p| p.port == *n),
        PortRef::Named(n) => svc.ports.iter().find(|p| p.name.as_deref() == Some(n)),
    };
    let Some(svc_port) = svc_port else {
        return Vec::new();
    };
    // Absent targetPort = identity (the Service port number), k8s semantics.
    let target = svc_port
        .target_port
        .clone()
        .unwrap_or(TargetPort::Number(svc_port.port));
    let Some(ep) = state.endpoints.get(key) else {
        return Vec::new(); // missing Endpoints -> proxy 503
    };
    let mut peers = Vec::new();
    for subset in &ep.subsets {
        let Some(entry) = subset.ports.iter().find(|e| match &target {
            TargetPort::Number(p) => e.port == *p,
            TargetPort::Named(n) => e.name.as_deref() == Some(n),
        }) else {
            continue;
        };
        for ip in &subset.addresses {
            if let Ok(ip) = ip.parse::<IpAddr>() {
                peers.push(SocketAddr::new(ip, entry.port));
            }
        }
    }
    peers
}

/// Cheaply-clonable live resolver: a watch receiver over folded state.
#[derive(Clone, Debug)]
pub struct EndpointsResolver {
    pub(crate) rx: watch::Receiver<Arc<ResolverState>>,
}

impl EndpointsResolver {
    /// The current state snapshot.
    pub fn snapshot(&self) -> Arc<ResolverState> {
        self.rx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<ResolverState>> {
        self.rx.clone()
    }
}

impl UpstreamResolver for EndpointsResolver {
    fn resolve(&self, upstream: &UpstreamRef) -> Vec<SocketAddr> {
        resolve_with_state(&self.rx.borrow(), upstream)
    }
}

#[cfg(test)]
mod tests;
