//! Embedded **ServiceLB** L4 data plane (**T5.6 / T4.3 Scope B**, decision
//! **Q41**): for `type=LoadBalancer` Services whose
//! `status.loadBalancer.ingress[0].ip` VIP is announced, host a RAW TCP
//! byte-pipe listener at `vip:servicePort` round-robining across the
//! Service's ready Endpoints. No HTTP anywhere — that stays the nodePort
//! gateway's job ([`crate::nodeport`], which since Q41 admits
//! LoadBalancer-type Services on the nodePort plane too, because an LB
//! builds on NodePort in k8s).
//!
//! Reconcile model mirrors the nodePort plane: one dedicated worker thread
//! hosting a current-thread runtime, re-driven on every Services/Endpoints
//! fold ([`crate::endpoints::EndpointsResolver`]). Deliberate differences:
//!
//! - **bind failure = announce-only**: the announced VIP usually is NOT
//!   local (it belongs to the LB provider); a failed bind logs ONE warning
//!   per desired listener key (never spams) and skips — reconcile retries
//!   only when the desired key set changes.
//! - **no HTTP 503-while-empty**: an accepted connection with an empty
//!   backend snapshot gets an immediate FIN (write-side shutdown + drop) —
//!   the client sees EOF and must not be held open.
//! - **drain aborts accept loops only**: per-connection byte-pipes are
//!   plain spawned tasks on the plane runtime; they die with the runtime
//!   when [`ServiceLbPlane::drain`] joins the worker thread (same posture
//!   as [`crate::serve`]'s in-flight connections).

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use infra::Shutdown;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::endpoints::{resolve_with_state, EndpointsResolver, ResolverState};
use crate::route::{PortRef, UpstreamRef};

/// One desired LB listener: the `vip:servicePort` bind key, the backend
/// selection (`ns/name` + Service port) and the peer snapshot the fold
/// produced. `peers` is informational — connections re-resolve live at
/// accept time, so Endpoints churn never bounces the listener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LbListener {
    pub listen: SocketAddr,
    pub service: String,
    pub port: u16,
    pub peers: Vec<SocketAddr>,
}

/// Pure desired-set computation (unit-testable, no I/O): for every
/// `LoadBalancer`-type Service with a parseable announced VIP, one entry
/// per TCP Service port — `listen = (ingress_ip, port)`, backends = the
/// ready endpoint addresses for that port (same resolution seam as the
/// nodePort plane, [`resolve_with_state`]).
pub(crate) fn desired_lb_listeners(state: &ResolverState) -> Vec<LbListener> {
    let mut out = Vec::new();
    for (key, svc) in &state.services {
        if svc.kind_type != "LoadBalancer" {
            continue;
        }
        // Skip unannounced / unparseable VIPs: without a local address to
        // serve on there is no byte-pipe to host (announce-only mode).
        let Some(ip) = svc
            .ingress_ip
            .as_deref()
            .and_then(|s| s.parse::<IpAddr>().ok())
        else {
            continue;
        };
        for p in &svc.ports {
            // L4 TCP only: UDP ServiceLB is out of scope (Q41).
            if p.protocol.as_deref() == Some("UDP") {
                continue;
            }
            let upstream = UpstreamRef {
                service: key.clone(),
                port: PortRef::Number(p.port),
            };
            out.push(LbListener {
                listen: SocketAddr::new(ip, p.port),
                service: key.clone(),
                port: p.port,
                peers: resolve_with_state(state, &upstream),
            });
        }
    }
    out
}

/// One live LB listener: accept task + its round-robin cursor.
struct Listener {
    /// Backend selection identity (`ns/name` + Service port).
    service: String,
    port: u16,
    rr: Arc<AtomicUsize>,
    shutdown: Shutdown,
    task: JoinHandle<()>,
}

/// The ServiceLB plane handle; [`Self::drain`] awaits full teardown.
pub struct ServiceLbPlane {
    thread: std::thread::JoinHandle<()>,
}

impl ServiceLbPlane {
    /// Abort every accept loop, join the worker thread (in-flight
    /// byte-pipes die with the runtime — see the module docs).
    pub async fn drain(self) {
        let _ = tokio::task::spawn_blocking(move || self.thread.join()).await;
    }
}

/// Spawn the ServiceLB plane on its dedicated worker thread (mirrors
/// [`crate::nodeport::spawn`]).
pub fn spawn(resolver: EndpointsResolver, shutdown: Shutdown) -> ServiceLbPlane {
    let thread = std::thread::Builder::new()
        .name("servicelb-plane".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("ServiceLB plane runtime");
            rt.block_on(reconcile_loop(resolver, shutdown));
        })
        .expect("ServiceLB plane thread");
    ServiceLbPlane { thread }
}

/// The plane's main loop: reconcile on every fold, retire on shutdown.
async fn reconcile_loop(resolver: EndpointsResolver, shutdown: Shutdown) {
    let mut listeners: BTreeMap<SocketAddr, Listener> = BTreeMap::new();
    let mut reaped: Vec<JoinHandle<()>> = Vec::new();
    let mut warned: BTreeSet<SocketAddr> = BTreeSet::new();
    let mut state_rx = resolver.subscribe();
    loop {
        {
            let state = state_rx.borrow_and_update().clone();
            reconcile(&mut listeners, &mut reaped, &mut warned, &resolver, &state).await;
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

/// One reconcile step: drop stale listeners, start missing ones, warn once
/// per desired key whose VIP is not locally bindable (announce-only).
async fn reconcile(
    listeners: &mut BTreeMap<SocketAddr, Listener>,
    reaped: &mut Vec<JoinHandle<()>>,
    warned: &mut BTreeSet<SocketAddr>,
    resolver: &EndpointsResolver,
    state: &ResolverState,
) {
    let desired: BTreeMap<SocketAddr, LbListener> =
        desired_lb_listeners(state).into_iter().map(|l| (l.listen, l)).collect();
    reaped.retain(|h| !h.is_finished());
    warned.retain(|k| desired.contains_key(k));
    let stale: Vec<SocketAddr> = listeners
        .iter()
        .filter(|(k, l)| {
            !desired
                .get(*k)
                .is_some_and(|d| d.service == l.service && d.port == l.port)
        })
        .map(|(k, _)| *k)
        .collect();
    for key in stale {
        if let Some(l) = listeners.remove(&key) {
            tracing::info!(target: "init-pro", listen = %key, "ServiceLB listener retired");
            l.shutdown.trigger();
            reaped.push(l.task);
        }
    }
    for (key, d) in &desired {
        if listeners.contains_key(key) {
            continue;
        }
        match start_listener(*key, d, resolver).await {
            Some(l) => {
                tracing::info!(
                    target: "init-pro",
                    listen = %key,
                    service = %d.service,
                    peers = d.peers.len(),
                    "ServiceLB listener up"
                );
                warned.remove(key);
                listeners.insert(*key, l);
            }
            None => {
                // Announce-only: the VIP is not local. Warn ONCE per
                // desired key; a later successful bind clears the flag.
                if warned.insert(*key) {
                    tracing::warn!(
                        target: "init-pro",
                        listen = %key,
                        service = %d.service,
                        "ServiceLB VIP not bindable on this node; announce-only"
                    );
                }
            }
        }
    }
}

/// Bind `vip:port` and spawn the accept loop for the desired entry.
/// `None` = bind failed (caller warns once; announce-only mode).
async fn start_listener(
    listen: SocketAddr,
    target: &LbListener,
    resolver: &EndpointsResolver,
) -> Option<Listener> {
    let listener = TcpListener::bind(listen).await.ok()?;
    let rr = Arc::new(AtomicUsize::new(0));
    let shutdown = Shutdown::new();
    let task = tokio::spawn(accept_loop(
        listener,
        resolver.clone(),
        target.service.clone(),
        target.port,
        Arc::clone(&rr),
        shutdown.clone(),
    ));
    Some(Listener {
        service: target.service.clone(),
        port: target.port,
        rr,
        shutdown,
        task,
    })
}

/// Accept loop: every accepted connection becomes an independent
/// byte-pipe task (round-robin over the live backend snapshot).
async fn accept_loop(
    listener: TcpListener,
    resolver: EndpointsResolver,
    service: String,
    port: u16,
    rr: Arc<AtomicUsize>,
    shutdown: Shutdown,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((client, remote)) => {
                    let _ = client.set_nodelay(true);
                    tracing::debug!(
                        target: "init-pro", %remote, service = %service,
                        "ServiceLB connection"
                    );
                    let resolver = resolver.clone();
                    let service = service.clone();
                    let rr = Arc::clone(&rr);
                    tokio::spawn(async move {
                        proxy(client, &resolver, &service, port, &rr).await;
                    });
                }
                Err(e) => {
                    tracing::warn!(target: "init-pro", error = %e,
                        "ServiceLB accept failed");
                }
            }
        }
    }
}

/// One byte-pipe: snapshot backends, pick one round-robin, splice the
/// streams. Empty snapshot -> immediate FIN (read nothing, never hold).
async fn proxy(
    mut client: TcpStream,
    resolver: &EndpointsResolver,
    service: &str,
    port: u16,
    rr: &AtomicUsize,
) {
    let state = resolver.snapshot();
    let peers = resolve_with_state(
        &state,
        &UpstreamRef {
            service: service.to_string(),
            port: PortRef::Number(port),
        },
    );
    if peers.is_empty() {
        // No ready backends: EOF to the client, connection closed.
        let _ = client.shutdown().await;
        return;
    }
    let start = rr.fetch_add(1, Ordering::Relaxed);
    // One retry on the next peer (round-robin order), then give up.
    for i in 0..peers.len().min(2) {
        let peer = peers[(start + i) % peers.len()];
        match TcpStream::connect(peer).await {
            Ok(mut upstream) => {
                let _ = upstream.set_nodelay(true);
                // Byte-pipe until either side closes (or the plane drains).
                let _ = copy_bidirectional(&mut client, &mut upstream).await;
                return;
            }
            Err(e) => {
                tracing::debug!(target: "init-pro", peer = %peer, error = %e,
                    "ServiceLB backend connect failed; trying next");
            }
        }
    }
    let _ = client.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::{EndpointsView, EpPort, EpSubset, ServiceView, SvcPort};

    fn svc(kind: &str, ingress_ip: Option<&str>, ports: Vec<SvcPort>) -> ServiceView {
        ServiceView {
            namespace: "default".into(),
            name: "web".into(),
            kind_type: kind.into(),
            ports,
            ingress_ip: ingress_ip.map(str::to_string),
        }
    }

    fn svc_port(port: u16, protocol: Option<&str>) -> SvcPort {
        SvcPort {
            name: None,
            protocol: protocol.map(str::to_string),
            port,
            target_port: None,
            node_port: None,
        }
    }

    fn with_endpoints(state: &mut ResolverState, addrs: &[&str], port: u16) {
        state.endpoints.insert(
            "default/web".into(),
            EndpointsView {
                subsets: vec![EpSubset {
                    addresses: addrs.iter().map(|a| (*a).to_string()).collect(),
                    ports: vec![EpPort { name: None, port }],
                }],
            },
        );
    }

    fn desired(state: &ResolverState) -> Vec<LbListener> {
        let mut v = desired_lb_listeners(state);
        v.sort_by_key(|l| l.listen.to_string());
        v
    }

    #[test]
    fn desired_lb_listener_per_announced_port() {
        let mut st = ResolverState::default();
        st.services.insert(
            "default/web".into(),
            svc(
                "LoadBalancer",
                Some("192.0.2.10"),
                vec![svc_port(80, None), svc_port(443, None)],
            ),
        );
        with_endpoints(&mut st, &["10.0.0.5", "10.0.0.6"], 80);
        let d = desired(&st);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].listen, "192.0.2.10:80".parse().unwrap());
        assert_eq!(d[0].port, 80);
        // Backends = ready endpoint addresses for that Service port.
        assert_eq!(d[0].peers.len(), 2);
        assert!(d[0].peers.contains(&"10.0.0.5:80".parse().unwrap()));
        assert_eq!(d[1].listen, "192.0.2.10:443".parse().unwrap());
        // 443 has no matching endpoint port -> no backends (announce-only
        // listener; connections get an immediate FIN).
        assert!(d[1].peers.is_empty());
    }

    #[test]
    fn desired_skips_unannounced_unparseable_and_non_lb() {
        let mut st = ResolverState::default();
        st.services.insert(
            "default/novip".into(),
            svc("LoadBalancer", None, vec![svc_port(80, None)]),
        );
        st.services.insert(
            "default/badvip".into(),
            svc("LoadBalancer", Some("not-an-ip"), vec![svc_port(80, None)]),
        );
        st.services.insert(
            "default/cip".into(),
            svc("ClusterIP", Some("192.0.2.10"), vec![svc_port(80, None)]),
        );
        st.services.insert(
            "default/np".into(),
            svc("NodePort", Some("192.0.2.10"), vec![svc_port(80, None)]),
        );
        assert!(desired_lb_listeners(&st).is_empty());
    }

    #[test]
    fn desired_skips_udp_ports() {
        let mut st = ResolverState::default();
        st.services.insert(
            "default/dns".into(),
            svc(
                "LoadBalancer",
                Some("192.0.2.10"),
                vec![svc_port(53, Some("UDP")), svc_port(53, None)],
            ),
        );
        let d = desired(&st);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].listen.port(), 53);
        assert_eq!(d[0].peers, Vec::new());
        assert_eq!(d[0].listen, "192.0.2.10:53".parse().unwrap());
    }
}
