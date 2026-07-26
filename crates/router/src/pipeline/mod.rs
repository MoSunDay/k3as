//! The request phase pipeline (TODO **T5.2**).
//!
//! For each incoming request the pipeline:
//! 1. builds a fresh [`RequestContext`] from the request head (+ buffered body);
//! 2. runs the **generative** phases (`rewrite` -> `access` -> `content`) as
//!    separate Lua coroutines on the worker VM — each sharing the same context
//!    via the Q13 coroutine-local store. Any phase may short-circuit via
//!    `ngx.exit`; `ngx.exec` re-dispatches (re-runs) for a new URI;
//! 3. runs the **filter** phases (`header_filter` -> `body_filter`), which always
//!    run, mutating the assembled headers/body;
//! 4. runs `log` (fire and forget) and returns the final [`PhaseOutcome`].
//!
//! # Per-phase coroutines (Q13)
//! Each phase is driven as its **own** explicit Lua `Thread` bound to the shared
//! request context. `current_thread().to_pointer()` resolves the right request
//! even under interleaving — distinct threads, distinct pointers, one shared
//! `Rc<RefCell<RequestContext>>`. (A single long-lived driver thread would
//! complicate `ngx.exit` error propagation; per-phase threads keep isolation
//! clean. ADR Q14.)

use std::cell::RefCell;
use std::rc::Rc;

use http::request::Parts;
use http::Request;
use mlua::{Function, Lua};

use crate::context::{self, RequestContext};
use crate::ngx::EXIT_SENTINEL;

pub(crate) mod outcome;
pub(crate) mod phase;

pub use outcome::{build_response, PhaseOutcome};
pub use phase::Phase;
use phase::PhaseList;

/// Cap on `ngx.exec` re-dispatches per request (openresty's loop guard).
const MAX_EXEC_REDIRECTS: u32 = 10;

/// A worker-wide Lua VM configured with an ordered set of phase functions.
///
/// `!Send`: one VM per worker thread (Q12). Drive on a `tokio::task::LocalSet`.
pub struct Pipeline {
    lua: Lua,
    phases: PhaseList,
    init_worker: Option<Function>,
}

/// Builder for a [`Pipeline`]: register optional phase functions, then build.
#[derive(Clone)]
pub struct PipelineBuilder {
    chunks: Vec<(Phase, String)>,
}

impl Pipeline {
    /// Build a content-only pipeline (the T5.2a convenience: a single
    /// `content_by_lua`). For the full phase chain use [`Pipeline::build`].
    pub fn new(content_by_lua: &str) -> mlua::Result<Self> {
        PipelineBuilder::new().content(content_by_lua).try_build()
    }

    /// Start a phase-chain builder.
    pub fn build() -> PipelineBuilder {
        PipelineBuilder::new()
    }

    /// Run the `init_worker` phase once (if registered). Call at boot, before
    /// serving requests. No request context is bound, so per-request `ngx.*`
    /// APIs error here (as in openresty).
    pub async fn boot(&self) {
        let Some(func) = &self.init_worker else {
            return;
        };
        match self.lua.create_thread(func.clone()) {
            Ok(thread) => match drive_thread(&thread).await {
                Ok(()) => {}
                Err(e) if is_exit(&e) => {}
                Err(e) => tracing::error!(
                    target: "init-pro",
                    error = %e,
                    "init_worker_by_lua raised"
                ),
            },
            Err(e) => tracing::error!(target: "init-pro", error = %e, "init_worker thread"),
        }
    }

    /// Run one request through the pipeline (no body). The body type `B` is
    /// discarded; for a request body use [`Self::serve_request_with_body`].
    pub async fn serve_request<B>(&self, req: Request<B>) -> PhaseOutcome {
        let (parts, _) = req.into_parts();
        self.serve_request_with_body(&parts, Vec::new()).await
    }

    /// Run one request (with its buffered body) through the full phase chain.
    pub async fn serve_request_with_body(&self, parts: &Parts, body: Vec<u8>) -> PhaseOutcome {
        self.run(parts, body).await
    }

    /// The phase-chain driver.
    async fn run(&self, parts: &Parts, body: Vec<u8>) -> PhaseOutcome {
        let ctx = Rc::new(RefCell::new(
            RequestContext::from_parts(parts).with_body(body),
        ));

        // --- generative phases, with ngx.exec re-dispatch loop ---
        let mut execs = 0u32;
        loop {
            self.run_generative(&ctx).await;
            // Clone out of the RefCell before the match so the immutable borrow
            // is dropped before reset_for_exec takes a mutable one.
            let exec_uri = ctx.borrow().exec_uri.clone();
            match exec_uri {
                Some(new_uri) if execs < MAX_EXEC_REDIRECTS => {
                    execs += 1;
                    ctx.borrow_mut().reset_for_exec(new_uri);
                    continue;
                }
                Some(_) => {
                    tracing::warn!(
                        target: "init-pro",
                        max = MAX_EXEC_REDIRECTS,
                        "ngx.exec loop guard hit"
                    );
                    let mut g = ctx.borrow_mut();
                    g.exec_uri = None;
                    g.exit_code = Some(500);
                    g.status = 500;
                }
                None => {}
            }
            break;
        }

        // --- filter phases (always run, even after a short-circuit) ---
        self.run_header_filter(&ctx).await;
        self.run_body_filter(&ctx).await;

        // capture the response BEFORE log (log is fire-and-forget).
        let outcome = PhaseOutcome::from_ctx(&ctx);

        // --- log phase (post-response, outcome unaffected) ---
        if let Some(func) = self.phases.get(Phase::Log) {
            self.drive(&ctx, func, Phase::Log).await;
        }

        outcome
    }

    /// Run rewrite -> access -> content; stop as soon as a phase short-circuits
    /// via `ngx.exit` (exit_code set).
    async fn run_generative(&self, ctx: &Rc<RefCell<RequestContext>>) {
        for &phase in Phase::GENERATIVE.iter() {
            if ctx.borrow().exit_code.is_some() {
                break;
            }
            if let Some(func) = self.phases.get(phase) {
                self.drive(ctx, func, phase).await;
            }
        }
    }

    /// Run `header_filter_by_lua` (mutates `resp_headers` via `ngx.header`).
    async fn run_header_filter(&self, ctx: &Rc<RefCell<RequestContext>>) {
        if let Some(func) = self.phases.get(Phase::HeaderFilter) {
            self.drive(ctx, func, Phase::HeaderFilter).await;
        }
    }

    /// Run `body_filter_by_lua`: prime `ngx.arg[1]` with the assembled body,
    /// drive, then commit the (possibly transformed) chunk back as the body.
    /// Buffered whole-body transform (not true streaming) — documented ADR Q14.
    async fn run_body_filter(&self, ctx: &Rc<RefCell<RequestContext>>) {
        let Some(func) = self.phases.get(Phase::BodyFilter) else {
            return;
        };
        {
            let mut g = ctx.borrow_mut();
            g.arg_body = g.body.clone();
            g.arg_eof = true;
        }
        self.drive(ctx, func, Phase::BodyFilter).await;
        let mut g = ctx.borrow_mut();
        g.body = std::mem::take(&mut g.arg_body);
    }

    /// Drive one phase function as its own coroutine, bound to the shared
    /// context. `ngx.exit` (sentinel error + exit_code) is treated as a clean
    /// phase end; any other Lua error is logged.
    async fn drive(&self, ctx: &Rc<RefCell<RequestContext>>, func: &Function, phase: Phase) {
        let thread = match self.lua.create_thread(func.clone()) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(target: "init-pro", phase = phase.label(), error = %e, "thread create");
                return;
            }
        };
        let ptr = thread.to_pointer() as usize;
        context::bind(&self.lua, ptr, ctx.clone());

        let result = drive_thread(&thread).await;

        context::unbind(&self.lua, ptr);
        match result {
            Ok(()) => {}
            Err(e) if is_exit(&e) => {}
            Err(e) => {
                tracing::error!(
                    target: "init-pro",
                    phase = phase.label(),
                    error = %e,
                    "phase raised"
                );
            }
        }
    }
}

impl PipelineBuilder {
    /// Start with no phases registered.
    pub fn new() -> Self {
        Self { chunks: Vec::new() }
    }

    /// Register a Lua chunk for a phase (the chunk must evaluate to a function).
    pub fn phase(mut self, phase: Phase, src: &str) -> Self {
        self.chunks.push((phase, src.to_owned()));
        self
    }

    /// Convenience: `phase(Phase::InitWorker, src)`.
    pub fn init_worker(self, src: &str) -> Self {
        self.phase(Phase::InitWorker, src)
    }
    /// Convenience: `phase(Phase::Rewrite, src)`.
    pub fn rewrite(self, src: &str) -> Self {
        self.phase(Phase::Rewrite, src)
    }
    /// Convenience: `phase(Phase::Access, src)`.
    pub fn access(self, src: &str) -> Self {
        self.phase(Phase::Access, src)
    }
    /// Convenience: `phase(Phase::Content, src)`.
    pub fn content(self, src: &str) -> Self {
        self.phase(Phase::Content, src)
    }
    /// Convenience: `phase(Phase::HeaderFilter, src)`.
    pub fn header_filter(self, src: &str) -> Self {
        self.phase(Phase::HeaderFilter, src)
    }
    /// Convenience: `phase(Phase::BodyFilter, src)`.
    pub fn body_filter(self, src: &str) -> Self {
        self.phase(Phase::BodyFilter, src)
    }
    /// Convenience: `phase(Phase::Log, src)`.
    pub fn log(self, src: &str) -> Self {
        self.phase(Phase::Log, src)
    }
    /// Convenience: `phase(Phase::Balancer, src)` — `balancer_by_lua` (T5.4).
    pub fn balancer(self, src: &str) -> Self {
        self.phase(Phase::Balancer, src)
    }
    /// Convenience: `phase(Phase::SslCertificate, src)` — `ssl_certificate_by_lua`
    /// (T5.4 Scope B). Stored for the dynamic-issuance path; M1 uses the Rust
    /// SNI cert resolver directly.
    pub fn ssl_certificate(self, src: &str) -> Self {
        self.phase(Phase::SslCertificate, src)
    }

    /// Evaluate every registered chunk and assemble the [`Pipeline`].
    pub fn try_build(self) -> mlua::Result<Pipeline> {
        let lua = crate::worker_vm()?;
        let mut phases = PhaseList::default();
        let mut init_worker = None;
        for (phase, src) in self.chunks {
            let func = lua.load(&src).eval::<Function>()?;
            if phase == Phase::InitWorker {
                init_worker = Some(func);
            } else {
                phases.set(phase, func);
            }
        }
        Ok(Pipeline {
            lua,
            phases,
            init_worker,
        })
    }
}

impl Default for PipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Drive a Lua coroutine thread to completion via `into_async`.
async fn drive_thread(thread: &mlua::Thread) -> mlua::Result<()> {
    let fut = thread.clone().into_async::<()>(())?;
    fut.await
}

/// True when an error is the `ngx.exit`/`ngx.exec`/`ngx.redirect` sentinel, or
/// the context already recorded an exit code (clean phase termination).
fn is_exit(e: &mlua::Error) -> bool {
    matches!(e, mlua::Error::RuntimeError(m) if m == EXIT_SENTINEL)
}
