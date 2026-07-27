//! Round-robin upstream load balancer + the upstream resolver trait (T5.4).
//!
//! The data plane matches a route to an [`UpstreamRef`] (Service + port), the
//! [`UpstreamResolver`] expands that to concrete peer addresses, and the
//! [`Balancer`] picks one peer per request via round-robin. `least-conn` and
//! pluggable strategies arrive in Scope B; the Lua `balancer_by_lua` hook
//! ([`crate::pipeline::Phase::Balancer`]) may override the selection later.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;

use crate::route::UpstreamRef;

/// Expands an [`UpstreamRef`] (Service + port) to live peer addresses.
///
/// In a real cluster this reads `Endpoints`/`EndpointSlice` for the Service;
/// Scope A ships [`StaticResolver`] (an in-process map) so the data plane can be
/// exercised without etcd/watch.
pub trait UpstreamResolver {
    fn resolve(&self, upstream: &UpstreamRef) -> Vec<SocketAddr>;
}

/// An in-process, pre-populated resolver: `UpstreamRef` → fixed peer list.
/// The stub "config source" for Scope A (no informer/watch dependency).
#[derive(Debug, Default)]
pub struct StaticResolver {
    peers: HashMap<UpstreamRef, Vec<SocketAddr>>,
}

impl StaticResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the peer addresses for an upstream.
    pub fn set(&mut self, upstream: UpstreamRef, peers: Vec<SocketAddr>) -> &mut Self {
        self.peers.insert(upstream, peers);
        self
    }
}

impl UpstreamResolver for StaticResolver {
    fn resolve(&self, upstream: &UpstreamRef) -> Vec<SocketAddr> {
        self.peers.get(upstream).cloned().unwrap_or_default()
    }
}

/// A stateless-by-key round-robin balancer: one rotating index per upstream.
///
/// `!Send` (uses `RefCell`): driven on a single `LocalSet`, shared across
/// per-connection tasks via `Rc`.
#[derive(Debug, Default)]
pub struct Balancer {
    counters: RefCell<HashMap<UpstreamRef, usize>>,
}

impl Balancer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pick the next peer for `upstream` via round-robin over `peers`.
    /// Returns `None` when the pool is empty.
    pub fn select(&self, upstream: &UpstreamRef, peers: &[SocketAddr]) -> Option<SocketAddr> {
        if peers.is_empty() {
            return None;
        }
        let mut counters = self.counters.borrow_mut();
        let idx = counters.entry(upstream.clone()).or_insert(0);
        let peer = peers[*idx % peers.len()];
        *idx = (*idx + 1) % peers.len();
        Some(peer)
    }
}

/// Convenience: resolve then select in one call.
pub fn pick_peer(
    balancer: &Balancer,
    resolver: &dyn UpstreamResolver,
    upstream: &UpstreamRef,
) -> Option<SocketAddr> {
    let peers = resolver.resolve(upstream);
    balancer.select(upstream, &peers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::PortRef;
    use std::net::{IpAddr, Ipv4Addr};

    fn addr(p: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), p)
    }

    #[test]
    fn round_robin_cycles_peers() {
        let b = Balancer::new();
        let peers = vec![addr(1), addr(2), addr(3)];
        let up = UpstreamRef::port("svc", 80);
        assert_eq!(b.select(&up, &peers), Some(addr(1)));
        assert_eq!(b.select(&up, &peers), Some(addr(2)));
        assert_eq!(b.select(&up, &peers), Some(addr(3)));
        assert_eq!(b.select(&up, &peers), Some(addr(1)));
    }

    #[test]
    fn empty_pool_yields_none() {
        let b = Balancer::new();
        let up = UpstreamRef {
            service: "x".into(),
            port: PortRef::Number(1),
        };
        assert_eq!(b.select(&up, &[]), None);
    }

    #[test]
    fn static_resolver_returns_registered_peers() {
        let up = UpstreamRef::port("svc", 80);
        let mut r = StaticResolver::new();
        r.set(up.clone(), vec![addr(9)]);
        assert_eq!(r.resolve(&up), vec![addr(9)]);
        let missing = UpstreamRef::port("nope", 80);
        assert!(r.resolve(&missing).is_empty());
    }
}
