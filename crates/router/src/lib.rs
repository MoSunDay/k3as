//! Built-in Router data plane — openrusty-powered h2c gateway (workstream A,
//! decision **Q32**; externally observable behavior parity under **Q28**).
//!
//! The mlua Lua plane (T5.1 bridge, T5.2 phase pipeline, T5.3 resty.*,
//! T5.4 Ingress->Lua routing + TLS + Lua hot reload) is replaced by the
//! openrusty gateway engine: this crate now owns
//!
//! - the **reflectors**: Services + Endpoints ([`endpoints_watch`], S4) and
//!   the `networking.k8s.io/v1` Ingress set ([`ingress_watch`]), folded into
//!   one [`EndpointsResolver`];
//! - the **route model**: [`route::RouteTable`] — the Ingress compiler
//!   ([`ingress`]) target, still the semantic reference for host × path →
//!   upstream matching;
//! - the **gateways**: per-`nodePort` listeners ([`nodeport`]), the
//!   ClusterIP VIP listeners ([`clusterip`], T4.3 Scope C / **Q44** —
//!   VIPs aliased on `lo`, sharing the same resolver+gate+split seams),
//!   and the Ingress gateway ([`gateway`]), all openrusty `AppState`s
//!   served through the 503 gate ([`gate`]) over real HTTP/1.1 + h2c
//!   connections ([`serve`]), with openrusty's in-memory reload for live
//!   endpoint and Ingress changes (the Lua-era "hot reload" seam, now
//!   config-driven).
//!
//! Deviations from the Lua era are documented per-module (wildcard hosts,
//! TLS termination, active health probes); the wire behavior gated by
//! `scripts/service-traffic-e2e.sh` (ST1-ST6) is preserved.
//!
//! Threading: the reflectors ([`endpoints_watch::supervise`]) run on the
//! CLI `server` runtime, which is a multi-thread runtime
//! (`cli/src/runtime.rs`, `new_multi_thread`; spawned from
//! `cli/src/runtime_server.rs`). Only the listener planes -- NodePort
//! ([`spawn_nodeport_plane`]) and ClusterIP ([`spawn_clusterip_plane`]) --
//! each get their own dedicated thread with a current-thread runtime;
//! every connection task is plain `Send`.

#![forbid(unsafe_code)]

mod clusterip;
#[cfg(test)]
mod clusterip_tests;
mod endpoints;
mod endpoints_watch;
mod gate;
mod gateway;
mod gateway_config;
mod ingress;
mod ingress_watch;
pub mod nodeport;
mod route;
mod serve;
mod split;
mod traffic_watch;
mod vip;

pub use clusterip::{spawn as spawn_clusterip_plane, ClusterIpPlane};
pub use endpoints::{
    resolve_with_state, EndpointsResolver, EndpointsView, ResolverState, ServiceView, SvcPort,
    TargetPort,
};
pub use endpoints_watch::{endpoints_prefix, services_prefix, supervise};
pub use ingress::compile_ingress;
pub use ingress_watch::{ingresses_prefix, IngressMap};
pub use nodeport::{spawn as spawn_nodeport_plane, NodePortConfig, NodePortPlane};
pub use route::{HostMatcher, PathMatcher, PortRef, RouteRule, RouteTable, UpstreamRef};
pub use split::{endpoint_weights, plan_for, PeerWeight, SplitBackendView, SplitView};
pub use traffic_watch::trafficsplits_prefix;
