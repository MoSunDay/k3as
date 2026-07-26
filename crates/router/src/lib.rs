//! Built-in Router VM (TODO **T5.1**).
//!
//! Feasibility spike: proves a Lua coroutine can yield at a Rust `await`
//! point on a Tokio runtime without blocking the worker — the make-or-break
//! of **Q4** (a Lua-driven Router). This crate is deliberately self-contained:
//! no `api` / `apiserver` dependency, no HTTP pipeline, no
//! cosocket. Those land in T5.2 / T5.3 / T5.4.
//!
//! # VM model (ADR **Q12**)
//!
//! Mirrors openresty's worker model: **one worker-wide LuaJIT VM** (via
//! `mlua`'s vendored LuaJIT) carrying **per-coroutine Lua threads**, driven as
//! Rust futures on a **single-thread async runtime** (`tokio::task::LocalSet`).
//! Concurrency comes from cooperative yielding at `await` points — exactly
//! like openresty's per-worker coroutine scheduler — not from OS threads. This
//! also sidesteps `Lua: !Send`: there is exactly one VM per thread, so no
//! cross-thread sharing is needed.
#![forbid(unsafe_code)]

mod ngx;
mod vm;

pub use vm::worker_vm;
