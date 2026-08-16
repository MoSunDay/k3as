//! Per-nodePort reverse-proxy listeners (Sprint 18 / **S4**, Q28 — recorded
//! S7): the kube-proxy-equivalent service plane. Watches the folded
//! [`crate::endpoints::ResolverState`] and, for every `NodePort`-type
//! Service, keeps one reverse-proxy listener bound per allocated `nodePort`,
//! backed by the live Endpoints resolver (empty Endpoints -> proxy 503).
//! Wired into the CLI `server` runtime by default; `--disable-kube-proxy`
//! turns it off (k3s-parity escape hatch).
//!
//! Threading: [`crate::proxy::serve_proxy`] `spawn_local`s every connection
//! (openresty worker model), so the whole plane — reconcile loop included —
//! runs on ONE dedicated thread with a single-thread runtime + `LocalSet`.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::rc::Rc;

use infra::Shutdown;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::balancer::Balancer;
use crate::endpoints::{EndpointsResolver, ResolverState};
use crate::proxy::{serve_proxy, ProxyOptions};
use crate::route::{PortRef, RouteTable, UpstreamRef};

/// Bind configuration for the nodePort listeners.
#[derive(Clone, Copy, Debug)]
pub struct NodePortConfig {
    /// Interface to bind nodePorts on (kube-proxy binds the node address;
    /// we bind all interfaces by default).
    pub addr: IpAddr,
}

impl Default for NodePortConfig {
    fn default() -> Self {
        Self {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        }
    }
}

impl NodePortConfig {
    /// Loopback-only binding (integration tests, live-verify scripts).
    pub fn loopback() -> Self {
        Self {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        }
    }
}

/// One live nodePort listener.
struct Listener {
    upstream: UpstreamRef,
    shutdown: Shutdown,
    task: JoinHandle<()>,
}

/// Spawn the nodePort service plane on its dedicated worker thread.
///
/// The thread hosts a single-thread runtime + `LocalSet` driving the
/// reconcile loop, which starts/stops listeners to track the desired set.
/// [`NodePortPlane::drain`] awaits full teardown (call during shutdown).
pub fn spawn(
    resolver: EndpointsResolver,
    config: NodePortConfig,
    shutdown: Shutdown,
) -> NodePortPlane {
    let thread = std::thread::Builder::new()
        .name("nodeport-plane".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("nodePort plane runtime");
            let local = tokio::task::LocalSet::new();
            rt.block_on(local.run_until(reconcile_loop(resolver, config, shutdown)));
        })
        .expect("nodePort plane thread");
    NodePortPlane { thread }
}

/// The service-plane handle; [`Self::drain`] awaits full listener teardown.
pub struct NodePortPlane {
    thread: std::thread::JoinHandle<()>,
}

impl NodePortPlane {
    /// Stop every listener, join their tasks, then join the worker thread.
    pub async fn drain(self) {
        let _ = tokio::task::spawn_blocking(move || self.thread.join()).await;
    }
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

fn same_upstream(a: &UpstreamRef, b: &UpstreamRef) -> bool {
    a.service == b.service && a.port == b.port
}

async fn reconcile_loop(resolver: EndpointsResolver, config: NodePortConfig, shutdown: Shutdown) {
    let mut rx = resolver.subscribe();
    let mut listeners: BTreeMap<u16, Listener> = BTreeMap::new();
    let mut reaped: Vec<JoinHandle<()>> = Vec::new();
    loop {
        reconcile(
            &mut listeners,
            &mut reaped,
            config,
            &resolver,
            &rx.borrow_and_update(),
        )
        .await;
        tokio::select! {
            _ = shutdown.cancelled() => break,
            changed = rx.changed() => {
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

/// One reconcile step: drop stale listeners, start missing ones.
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
            tracing::info!(node_port = np, "nodePort listener retired");
            l.shutdown.trigger();
            reaped.push(l.task);
        }
    }
    for (np, up) in &desired {
        if listeners.contains_key(np) {
            continue;
        }
        match start_listener(config.addr, *np, up.clone(), resolver.clone()).await {
            Some(l) => {
                tracing::info!(node_port = np, service = %up.service, "nodePort listener up");
                listeners.insert(*np, l);
            }
            None => tracing::warn!(
                node_port = np,
                "nodePort bind failed; retrying on next state change"
            ),
        }
    }
}

/// Bind `addr:node_port` and spawn the proxy on the plane's LocalSet.
async fn start_listener(
    addr: IpAddr,
    node_port: u16,
    upstream: UpstreamRef,
    resolver: EndpointsResolver,
) -> Option<Listener> {
    let listener = TcpListener::bind(SocketAddr::new(addr, node_port))
        .await
        .ok()?;
    let shutdown = Shutdown::new();
    let child = shutdown.clone();
    let view = upstream.clone();
    let task = tokio::task::spawn_local(async move {
        let routes = RouteTable::new().with_default(upstream).finalise();
        let opts = ProxyOptions {
            balancer: Rc::new(Balancer::new()),
            resolver,
            pipeline: None,
            tls: None,
            reload: None,
        };
        if let Err(e) = serve_proxy(routes, opts, listener, child.cancelled()).await {
            tracing::warn!(node_port, error = %e, "nodePort listener exited");
        }
    });
    Some(Listener {
        upstream: view,
        shutdown,
        task,
    })
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
}
