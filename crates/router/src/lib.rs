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
//! - the **gateways**: per-`nodePort` listeners ([`nodeport`]) and the
//!   Ingress gateway ([`gateway`]), both openrusty `AppState`s served
//!   through the 503 gate ([`gate`]) over real HTTP/1.1 + h2c connections
//!   ([`serve`]), with openrusty's in-memory reload for live endpoint and
//!   Ingress changes (the Lua-era "hot reload" seam, now config-driven).
//!
//! Deviations from the Lua era are documented per-module (wildcard hosts,
//! TLS termination, active health probes); the wire behavior gated by
//! `scripts/service-traffic-e2e.sh` (ST1-ST6) is preserved.
//!
//! Threading: the whole plane runs on ONE dedicated thread with a
//! current-thread runtime (`router::supervise` + `router::spawn_nodeport_plane`
//! from the CLI `server` runtime); every connection task is plain `Send`.

#![forbid(unsafe_code)]

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

pub use endpoints::{
    resolve_with_state, EndpointsResolver, EndpointsView, ResolverState, ServiceView, SvcPort,
    TargetPort,
};
pub use endpoints_watch::{endpoints_prefix, services_prefix, supervise};
pub use ingress::compile_ingress;
pub use ingress_watch::{ingresses_prefix, IngressMap};
pub use nodeport::{spawn as spawn_nodeport_plane, NodePortConfig, NodePortPlane};
pub use route::{HostMatcher, PathMatcher, PortRef, RouteRule, RouteTable, UpstreamRef};
