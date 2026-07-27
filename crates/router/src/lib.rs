//! Built-in Router VM: a Lua-driven HTTP data plane.
//!
//! - **T5.1** (done): the mlua coroutine<->async bridge — a Lua coroutine
//!   yields at a Rust `await` point on a Tokio `LocalSet`, letting other
//!   coroutines run concurrently on the same worker VM (openresty's model,
//!   reproduced in Rust). ADR **Q12**.
//! - **T5.2** (done): the full openresty phase pipeline — `rewrite` ->
//!   `access` -> `content` -> `header_filter` -> `body_filter` -> `log`
//!   (+ `init_worker` at boot), with `ngx.req`/`ngx.header`/`ngx.status`/
//!   `ngx.var`/`ngx.exec`/`ngx.redirect`/`ngx.arg`, request-body reading and a
//!   real-TCP HTTP/1.1 data plane. A `header_filter_by_lua` mutates headers
//!   observed by a real client (the T5.2 acceptance gate). ADRs **Q13/Q14**.
//! - **T5.3** (done): `resty::*` + `ngx.shared.DICT`.
//! - **T5.4** (done): the Ingress→route-table compiler, a Rust round-robin
//!   [`balancer::Balancer`] + upstream resolver, a Rust HTTP reverse proxy
//!   ([`proxy::serve_proxy`]), TLS termination with SNI ([`tls`]), and a
//!   no-restart hot-reload seam ([`config`]). `Phase::Balancer` is wired.
//!
//! Cosocket (`ngx.socket.tcp`), TLS/SNI, and hot reload are all in. Live
//! `kube-rs` informer config sourcing + dynamic Lua cert issuance are deferred
//! to T5.5 / Phase 2.
#![forbid(unsafe_code)]

mod balancer;
mod config;
mod conn;
mod context;
mod cosocket;
mod ingress;
mod ngx;
mod pipeline;
mod proxy;
mod resty;
mod route;
mod serve;
mod tls;
mod vm;

pub use balancer::{pick_peer, Balancer, StaticResolver, UpstreamResolver};
pub use config::{reload_channel, ConfigSource, RouteStore, StaticConfigSource};
pub use context::RequestContext;
pub use ingress::compile_ingress;
pub use pipeline::{build_response, Phase, PhaseOutcome, Pipeline, PipelineBuilder};
pub use proxy::{serve_proxy, ProxyOptions};
pub use route::{HostMatcher, PathMatcher, PortRef, RouteRule, RouteTable, UpstreamRef};
pub use serve::{ephemeral_listener, serve};
pub use tls::{build_server_config, CertKey, SniCertResolver};
pub use vm::worker_vm;
