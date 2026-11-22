//! ClusterIP VIP listener plane (TODO **T4.3 Scope C**, decision **Q44**,
//! Sprint 51): for every Service with an allocated `spec.clusterIP`, keep
//! one gateway listener bound per `(VIP, servicePort)` — the VIP aliased
//! onto `lo` by [`crate::vip`] (klipper precedent), the listener driven by
//! the SAME openrusty/gate/split machinery as the nodePort plane
//! ([`crate::nodeport`], which this module is modeled after).
//!
//! Scope notes (Q44): TrafficSplit folding reuses [`crate::split::plan_for`]
//! exactly like nodeport (**Q57**); UDP is skipped; headless Services have
//! no IP; VIP scope is single-node; an aliasing failure (no `ip(1)` /
//! `NET_ADMIN`) logs ONE warn per VIP and skips that listener — degraded,
//! never fatal. The listener struct is a slimmer copy of nodeport.rs's
//! private one on purpose (Q28 wire behavior gated by ST1-ST6): this plane
//! shares the seams (gate, split, serve), not code.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use infra::Shutdown;
use openrusty_server::state::{self as or_state, AppState};
use tokio::task::JoinHandle;

use crate::endpoints::{EndpointsResolver, ResolverState};
use crate::gate::{GateRoute, GatedRouter};
use crate::gateway_config;
use crate::route::{PortRef, UpstreamRef};
use crate::serve;
use crate::split::PeerWeight;
use crate::vip;

/// One live VIP listener: bound address, openrusty runtime (hot-reloaded
/// on endpoint changes), the weighted peer set baked into it (empty = gate
/// 503s), and the gate's live target set (entry Service or split backends).
struct Listener {
    listen: SocketAddr,
    upstream: UpstreamRef,
    app: Arc<AppState>,
    peers: Vec<PeerWeight>,
    targets_tx: tokio::sync::watch::Sender<Arc<Vec<UpstreamRef>>>,
    shutdown: Shutdown,
    task: JoinHandle<()>,
}

/// The ClusterIP service plane handle; [`ClusterIpPlane::drain`] awaits full
/// teardown (call during shutdown).
pub struct ClusterIpPlane {
    thread: std::thread::JoinHandle<()>,
}

/// Spawn the ClusterIP plane on its dedicated worker thread (single-threaded
/// runtime, same threading posture as the nodePort plane).
pub fn spawn(resolver: EndpointsResolver, shutdown: Shutdown) -> ClusterIpPlane {
    let thread = std::thread::Builder::new()
        .name("clusterip-plane".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("clusterIP plane runtime");
            rt.block_on(reconcile_loop(resolver, shutdown));
        })
        .expect("clusterIP plane thread");
    ClusterIpPlane { thread }
}

impl ClusterIpPlane {
    /// Stop every listener, remove every alias we added, join the thread.
    pub async fn drain(self) {
        let _ = tokio::task::spawn_blocking(move || self.thread.join()).await;
    }
}

/// The plane's main loop: reconcile on every fold, retire on shutdown.
async fn reconcile_loop(resolver: EndpointsResolver, shutdown: Shutdown) {
    let mut listeners: BTreeMap<SocketAddr, Listener> = BTreeMap::new();
    let mut reaped: Vec<JoinHandle<()>> = Vec::new();
    // VIPs aliased onto `lo`, refcounted by the listeners they back; drain
    // removes every alias we added (no stale `/32` on lo).
    let mut aliased: BTreeMap<Ipv4Addr, usize> = BTreeMap::new();
    // VIPs whose aliasing already logged its (single) warn.
    let mut warned: BTreeSet<Ipv4Addr> = BTreeSet::new();
    let mut state_rx = resolver.subscribe();
    loop {
        {
            let state = state_rx.borrow_and_update().clone();
            reconcile(
                &mut listeners,
                &mut reaped,
                &mut aliased,
                &mut warned,
                &resolver,
                &state,
            )
            .await;
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
    // Drain cleanup: every alias we added goes away with the plane.
    for ip in aliased.into_keys() {
        vip::remove_alias(ip).await;
    }
}

/// Desired listener keys are always IPv4 (built from `spec.clusterIP`),
/// but `SocketAddr::ip()` hands back a generic `IpAddr`; narrow it.
fn v4_of(sa: SocketAddr) -> Ipv4Addr {
    match sa.ip() {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => unreachable!("clusterIP listener keys are IPv4"),
    }
}

/// One reconcile step: refresh endpoint sets, drop stale listeners (and
/// aliases whose desired refcount hit 0), start missing ones.
async fn reconcile(
    listeners: &mut BTreeMap<SocketAddr, Listener>,
    reaped: &mut Vec<JoinHandle<()>>,
    aliased: &mut BTreeMap<Ipv4Addr, usize>,
    warned: &mut BTreeSet<Ipv4Addr>,
    resolver: &EndpointsResolver,
    state: &ResolverState,
) {
    let desired = desired_vip_listeners(state);
    // Desired refcount per VIP: how many listeners still want the alias.
    let mut want: BTreeMap<Ipv4Addr, usize> = BTreeMap::new();
    for sa in desired.keys() {
        *want.entry(v4_of(*sa)).or_insert(0) += 1;
    }
    reaped.retain(|h| !h.is_finished());
    let stale: Vec<SocketAddr> = listeners
        .iter()
        .filter(|(k, l)| !desired.get(k).is_some_and(|u| u == &l.upstream))
        .map(|(k, _)| *k)
        .collect();
    for k in stale {
        if let Some(l) = listeners.remove(&k) {
            tracing::info!(target: "init-pro", vip = %k.ip(), port = k.port(), "clusterIP listener retired");
            l.shutdown.trigger();
            reaped.push(l.task);
        }
    }
    // After retiring: drop aliases no desired listener needs (and we added).
    let dropped: Vec<Ipv4Addr> = aliased
        .keys()
        .filter(|ip| !want.contains_key(*ip))
        .copied()
        .collect();
    for ip in dropped {
        aliased.remove(&ip);
        vip::remove_alias(ip).await;
    }
    for (k, up) in &desired {
        match listeners.get_mut(k) {
            Some(l) => refresh_peers(l, resolver).await,
            None => {
                let ip = v4_of(*k);
                // Bind mechanism first (Q44): alias the VIP onto lo.
                match vip::ensure_alias(ip).await {
                    Ok(_) => {
                        warned.remove(&ip);
                        *aliased.entry(ip).or_insert(0) += 1;
                        match start_listener(*k, up.clone(), resolver).await {
                            Some(l) => {
                                tracing::info!(target: "init-pro", vip = %ip, port = k.port(), service = %up.service, "clusterIP listener up");
                                listeners.insert(*k, l);
                            }
                            None => {
                                tracing::warn!(target: "init-pro", vip = %ip, port = k.port(), "clusterIP bind failed; retrying on next state change")
                            }
                        }
                    }
                    Err(msg) => {
                        if warned.insert(ip) {
                            // ONE warn per VIP key; reconcile retries silently.
                            tracing::warn!(target: "init-pro", vip = %ip, error = %msg, "clusterIP alias failed; VIP listener skipped");
                        }
                    }
                }
            }
        }
    }
}

/// Hot-apply the plan's weighted peer set (same semantics as the nodePort
/// plane's `refresh_peers`: empty set keeps the last config and the gate
/// 503s; a changed non-empty set reloads atomically; the gate's target set
/// refreshes on every fold so split-backend-only edits still land, Q57).
async fn refresh_peers(listener: &mut Listener, resolver: &EndpointsResolver) {
    let state = resolver.snapshot();
    let plan = crate::split::plan_for(&state, &listener.upstream);
    if *listener.targets_tx.borrow().as_ref() != plan.targets {
        let _ = listener
            .targets_tx
            .send_replace(Arc::new(plan.targets.clone()));
    }
    if plan.peers == listener.peers {
        return;
    }
    if plan.peers.is_empty() {
        // Gate 503s from the live snapshot; keep the last valid config.
        listener.peers.clear();
        return;
    }
    let cfg = gateway_config::nodeport_config(listener.listen, &listener.upstream, &plan.peers);
    match openrusty_server::reload::apply_config(&listener.app, &cfg).await {
        Ok(generation) => {
            tracing::debug!(target: "init-pro", vip = %listener.listen.ip(), port = listener.listen.port(), peers = plan.peers.len(), generation, "clusterIP upstream peers re-applied");
            listener.peers = plan.peers;
        }
        Err(e) => tracing::warn!(target: "init-pro", error = %e, "clusterIP config reload failed"),
    }
}

/// Bind `VIP:port` and spawn the gated openrusty listener task.
async fn start_listener(
    listen: SocketAddr,
    upstream: UpstreamRef,
    resolver: &EndpointsResolver,
) -> Option<Listener> {
    let listener = serve::bind(listen).await.ok()?;
    let state = resolver.snapshot();
    let plan = crate::split::plan_for(&state, &upstream);
    let peers = plan.peers;
    let cfg = gateway_config::nodeport_config(listen, &upstream, &peers);
    let app = match or_state::from_config(
        cfg,
        std::path::PathBuf::from(format!("init-pro://router/clusterip/{}", listen)),
    ) {
        Ok(app) => app,
        Err(e) => {
            tracing::warn!(target: "init-pro", %listen, error = %e, "clusterIP runtime bootstrap failed");
            return None;
        }
    };
    let (targets_tx, targets_rx) = tokio::sync::watch::channel(Arc::new(plan.targets));
    let gated = GatedRouter::new(
        openrusty_server::app::router(app.clone()),
        GateRoute::Any(Arc::new(targets_rx)),
        resolver.clone(),
    );
    let shutdown = Shutdown::new();
    let task = tokio::spawn(serve::serve_until(listener, gated, shutdown.clone()));
    Some(Listener {
        listen,
        upstream,
        app,
        peers,
        targets_tx,
        shutdown,
        task,
    })
}

/// Desired listener set from a state snapshot: one entry per
/// `(clusterIP, TCP servicePort)` of every non-ExternalName Service with
/// an allocated VIP — upstream parity, every non-headless Service has one
/// (ClusterIP/NodePort/LoadBalancer alike).
pub fn desired_vip_listeners(state: &ResolverState) -> BTreeMap<SocketAddr, UpstreamRef> {
    let mut out = BTreeMap::new();
    for (key, svc) in &state.services {
        if svc.kind_type == "ExternalName" {
            continue; // forwards by externalName: no VIP
        }
        let Some(vip) = svc.cluster_ip else {
            continue; // headless (or no IP yet): contributes nothing
        };
        for p in &svc.ports {
            // Q44 subset-out: only TCP (default) service ports.
            let proto = p.protocol.as_deref().unwrap_or("TCP");
            if !proto.eq_ignore_ascii_case("TCP") {
                continue;
            }
            // The gate resolves peers by *Service* port (same rationale as
            // the nodePort plane: `resolve_with_state` matches
            // `PortRef::Number` against `svc_port.port` and maps targetPort
            // itself).
            out.insert(
                SocketAddr::new(IpAddr::V4(vip), p.port),
                UpstreamRef {
                    service: key.clone(),
                    port: PortRef::Number(p.port),
                },
            );
        }
    }
    out
}
