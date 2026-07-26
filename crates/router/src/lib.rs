//! Built-in Router VM: a Lua-driven HTTP data plane.
//!
//! - **T5.1** (done): the mlua coroutine<->async bridge — a Lua coroutine
//!   yields at a Rust `await` point on a Tokio `LocalSet`, letting other
//!   coroutines run concurrently on the same worker VM (openresty's model,
//!   reproduced in Rust). ADR **Q12**.
//! - **T5.2a** (this round): the request phase pipeline. A `content_by_lua`
//!   function reads the request and writes status + body, observed by a real
//!   HTTP client over TCP. Per-request binding is coroutine-local (ADR **Q13**).
//!
//! Cosocket (`ngx.socket.tcp`), the full phase order, and `resty::*` land in
//! T5.2b / T5.2c / T5.3.
#![forbid(unsafe_code)]

mod context;
mod cosocket;
mod ngx;
mod pipeline;
mod serve;
mod vm;

pub use context::RequestContext;
pub use pipeline::{build_response, PhaseOutcome, Pipeline};
pub use serve::{ephemeral_listener, serve};
pub use vm::worker_vm;
