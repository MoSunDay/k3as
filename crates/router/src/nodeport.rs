//! Per-nodePort gateway listeners (Sprint 18 / **S4**, Q28 — recorded S7;
//! re-platformed onto openrusty by workstream A / decision **Q32**): the
//! kube-proxy-equivalent service plane. Watches the folded
//! [`crate::endpoints::ResolverState`] and, for every `NodePort`-type
//! Service, keeps one gateway listener bound per allocated `nodePort`,
//! backed by the live Endpoints resolver (empty Endpoints -> the gate
//! answers 503, Q28 parity). Wired into the CLI `server` runtime by
//! default; `--disable-kube-proxy` turns it off (k3s-parity escape hatch).
//!
//! Externally observable parity with the Lua-era plane (Q28): same bind
//! model, same SWRR-style round-robin spread over ready peers (openrusty
//! SWRR), same 503-while-empty, same listener lifecycle on Service
//! create/delete/nodePort change. Deviations are documented in
//! [`crate::gateway_config`] (passive-only health) and [`crate::gate`].

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use infra::Shutdown;
use openrusty_server::state::{self as or_state, AppState};
use tokio::task::JoinHandle;

use crate::endpoints::{resolve_with_state, EndpointsResolver, ResolverState};
use crate::gate::{GateRoute, GatedRouter};
use crate::gateway_config;
use crate::route::{PortRef, UpstreamRef};
use crate::serve;

/// Bind configuration for the nodePort listeners.
#[derive(Clone, Copy, Debug)]
pub struct NodePortConfig {
    /// Interface to bind nodePorts on (kube-proxy binds the node address;
    /// we bind all interfaces by default).
    pub addr: IpAddr,
    /// Where to serve the Ingress gateway ([`crate::gateway`]). `None`
    /// (the default) keeps the gateway off; the CLI wires it when
    /// ingress serving is enabled (Q32).
    pub ingress_listen: Option<SocketAddr>,
}

impl Default for NodePortConfig {
    fn default() -> Self {
        Self {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ingress_listen: None,
        }
    }
}

impl NodePortConfig {
    /// Loopback-only binding (integration tests, live-verify scripts).
    pub fn loopback() -> Self {
        Self {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            ingress_listen: None,
        }
    }
}

/// One live nodePort listener: the openrusty state + its current peer set.
struct Listener {
    /// The bound address (`addr:nodePort`), for config regeneration.
    listen: SocketAddr,
    upstream: UpstreamRef,
    /// The applied openrusty runtime (hot-reloaded on endpoint changes).
    app: Arc<AppState>,
    /// The peer set currently baked into `app` (empty = gate 503s).
    peers: Vec<SocketAddr>,
    shutdown: Shutdown,
    task: JoinHandle<()>,
}

/// Spawn the gateway service plane on its dedicated worker thread.
///
/// The thread hosts a single-thread runtime driving the reconcile loop
/// (starts/stops listeners to track the desired set) and, when
/// `config.ingress_listen` is set, the Ingress gateway ([`crate::gateway`]).
/// [`NodePortPlane::drain`] awaits full teardown (call during shutdown).
pub fn spawn(
    resolver: EndpointsResolver,
    config: NodePortConfig,
    shutdown: Shutdown,
) -> NodePortPlane {
    let ingress_addr = Arc::new(std::sync::OnceLock::new());
    let ingress_handle = Arc::clone(&ingress_addr);
    let thread = std::thread::Builder::new()
        .name("nodeport-plane".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("nodePort plane runtime");
            rt.block_on(reconcile_loop(resolver, config, shutdown, ingress_handle));
        })
        .expect("nodePort plane thread");
    NodePortPlane {
        thread,
        ingress_addr,
    }
}

/// The service-plane handle; [`Self::drain`] awaits full listener teardown.
/// `ingress_addr` resolves to the bound Ingress address once the gateway is
/// up (needed because tests/scripts may pass `:0`).
pub struct NodePortPlane {
    thread: std::thread::JoinHandle<()>,
    ingress_addr: Arc<std::sync::OnceLock<SocketAddr>>,
}

impl NodePortPlane {
    /// The address the Ingress gateway bound (`None` while disabled or not
    /// yet listening).
    pub fn ingress_addr(&self) -> Option<SocketAddr> {
        self.ingress_addr.get().copied()
    }

    /// Stop every listener, join their tasks, then join the worker thread.
    pub async fn drain(self) {
        let _ = tokio::task::spawn_blocking(move || self.thread.join()).await;
    }
}

/// The plane's main loop: reconcile on every fold, retire on shutdown.
async fn reconcile_loop(
    resolver: EndpointsResolver,
    config: NodePortConfig,
    shutdown: Shutdown,
    ingress_addr: Arc<std::sync::OnceLock<SocketAddr>>,
) {
    let mut listeners: BTreeMap<u16, Listener> = BTreeMap::new();
    let mut reaped: Vec<JoinHandle<()>> = Vec::new();
    if let Some(listen) = config.ingress_listen {
        // Same dedicated thread/runtime: the gateway shares the resolver and
        // the shutdown; its serve task is dropped with the runtime on drain.
        tokio::spawn(crate::gateway::run(
            listen,
            resolver.clone(),
            shutdown.clone(),
            ingress_addr,
        ));
    }
    let mut state_rx = resolver.subscribe();
    loop {
        {
            let state = state_rx.borrow_and_update().clone();
            reconcile(&mut listeners, &mut reaped, config, &resolver, &state).await;
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            changed = state_rx.changed() => {
                if changed.is_err() {
                    break; // resolver channel dropped
                }
            }
        }
    }
    for (_, l) in std::mem::take(&mut listeners) {
        l.shutdown.trigger();
        reaped.push(l.task);
    }
    for task in reaped {
        let _ = task.await;
    }
}

/// One reconcile step: refresh endpoint sets, drop stale listeners, start
/// missing ones.
async fn reconcile(
    listeners: &mut BTreeMap<u16, Listener>,
    reaped: &mut Vec<JoinHandle<()>>,
    config: NodePortConfig,
    resolver: &EndpointsResolver,
    state: &ResolverState,
) {
    let desired = desired_listeners(state);
    reaped.retain(|h| !h.is_finished());
    let stale: Vec<u16> = listeners
        .iter()
        .filter(|(np, l)| {
            !desired
                .get(np)
                .is_some_and(|u| same_upstream(u, &l.upstream))
        })
        .map(|(np, _)| *np)
        .collect();
    for np in stale {
        if let Some(l) = listeners.remove(&np) {
            tracing::info!(target: "init-pro", node_port = np, "nodePort listener retired");
            l.shutdown.trigger();
            reaped.push(l.task);
        }
    }
    for (np, up) in &desired {
        match listeners.get_mut(np) {
            Some(l) => refresh_peers(l, resolver).await,
            None => match start_listener(config.addr, *np, up.clone(), resolver).await {
                Some(l) => {
                    tracing::info!(target: "init-pro", node_port = np, service = %up.service, "nodePort listener up");
                    listeners.insert(*np, l);
                }
                None => tracing::warn!(
                    target: "init-pro",
                    node_port = np,
                    "nodePort bind failed; retrying on next state change"
                ),
            },
        }
    }
}

/// Hot-apply the upstream's current peer set (openrusty in-memory reload).
/// An empty set keeps the last applied config — the [`crate::gate`] already
/// 503s it from the live snapshot, and openrusty rejects zero-peer
/// upstreams. A changed non-empty set is applied atomically (generation
/// bump, in-flight requests finish on the old snapshot).
async fn refresh_peers(listener: &mut Listener, resolver: &EndpointsResolver) {
    let state = resolver.snapshot();
    let peers = resolve_with_state(&state, &listener.upstream);
    if peers == listener.peers || peers.is_empty() {
        return;
    }
    let cfg = gateway_config::nodeport_config(listener.listen, &listener.upstream, &peers);
    match openrusty_server::reload::apply_config(&listener.app, &cfg).await {
        Ok(generation) => {
            tracing::debug!(
                target: "init-pro",
                generation,
                node_port = listener.listen.port(),
                peers = peers.len(),
                "nodePort endpoints re-applied"
            );
            listener.peers = peers;
        }
        Err(e) => {
            tracing::warn!(target: "init-pro", error = %e, "nodePort config reload failed")
        }
    }
}

/// Bind `addr:node_port` and spawn the gated openrusty listener task.
async fn start_listener(
    addr: IpAddr,
    node_port: u16,
    upstream: UpstreamRef,
    resolver: &EndpointsResolver,
) -> Option<Listener> {
    let listen = SocketAddr::new(addr, node_port);
    let listener = serve::bind(listen).await.ok()?;
    let state = resolver.snapshot();
    let peers = resolve_with_state(&state, &upstream);
    let cfg = gateway_config::nodeport_config(listen, &upstream, &peers);
    let app = match or_state::from_config(
        cfg,
        std::path::PathBuf::from(format!("init-pro://router/nodeport/{node_port}")),
    ) {
        Ok(app) => app,
        Err(e) => {
            tracing::warn!(target: "init-pro", node_port, error = %e, "nodePort runtime bootstrap failed");
            return None;
        }
    };
    let gated = GatedRouter::new(
        openrusty_server::app::router(app.clone()),
        GateRoute::Fixed(upstream.clone()),
        resolver.clone(),
    );
    let shutdown = Shutdown::new();
    let task = tokio::spawn(serve::serve_until(listener, gated, shutdown.clone()));
    Some(Listener {
        listen,
        upstream,
        app,
        peers,
        shutdown,
        task,
    })
}

/// Desired listener set from a state snapshot: one entry per allocated
/// `nodePort` of every NodePort-type Service.
fn desired_listeners(state: &ResolverState) -> BTreeMap<u16, UpstreamRef> {
    let mut out = BTreeMap::new();
    for (key, svc) in &state.services {
        if svc.kind_type != "NodePort" {
            continue;
        }
        for p in &svc.ports {
            if let Some(np) = p.node_port {
                // The gate resolves peers by *Service* port (`p.port`), not
                // targetPort: `resolve_with_state` matches `PortRef::Number`
                // against `svc_port.port` and maps targetPort -> endpoint
                // port itself. Naming the listener after targetPort made the
                // gate's port lookup miss and 503 forever.
                out.insert(
                    np,
                    UpstreamRef {
                        service: key.clone(),
                        port: PortRef::Number(p.port),
                    },
                );
            }
        }
    }
    out
}

/// Same Service + same port reference (listener reuse across reconciles).
fn same_upstream(a: &UpstreamRef, b: &UpstreamRef) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::{ServiceView, SvcPort};

    fn svc(kind: &str, ports: Vec<SvcPort>) -> ServiceView {
        ServiceView {
            namespace: "default".into(),
            name: "web".into(),
            kind_type: kind.into(),
            ports,
        }
    }

    fn svc_port(port: u16, np: Option<u16>) -> SvcPort {
        SvcPort {
            name: None,
            protocol: None,
            port,
            target_port: None,
            node_port: np,
        }
    }

    #[test]
    fn desired_only_nodeport_services_with_allocated_ports() {
        let mut st = ResolverState::default();
        st.services.insert(
            "default/web".into(),
            svc("NodePort", vec![svc_port(80, Some(30080))]),
        );
        st.services.insert(
            "default/ip".into(),
            svc("ClusterIP", vec![svc_port(443, None)]),
        );
        let d = desired_listeners(&st);
        assert_eq!(d.len(), 1);
        let up = &d[&30080];
        assert_eq!(up.service, "default/web");
        assert_eq!(up.port, PortRef::Number(80));
    }

    #[test]
    fn desired_supports_multiple_ports_per_service() {
        let mut st = ResolverState::default();
        st.services.insert(
            "default/multi".into(),
            svc(
                "NodePort",
                vec![svc_port(80, Some(30080)), svc_port(443, Some(30443))],
            ),
        );
        let d = desired_listeners(&st);
        assert_eq!(d.keys().copied().collect::<Vec<_>>(), vec![30080, 30443]);
    }

    /// NodePortConfig keeps the frozen CLI field and the opt-in gateway.
    #[test]
    fn config_defaults_keep_cli_contract() {
        let d = NodePortConfig::default();
        assert_eq!(d.addr, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert!(d.ingress_listen.is_none());
        assert!(NodePortConfig::loopback().ingress_listen.is_none());
    }
}
