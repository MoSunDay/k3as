//! The request phase pipeline (TODO **T5.2a**).
//!
//! For each incoming request the pipeline:
//! 1. builds a fresh [`RequestContext`] from the request head;
//! 2. creates an **explicit** Lua `Thread` for the phase function (Q13 — must
//!    be explicit so `Lua::current_thread()` resolves to *this* coroutine, not
//!    the implicit `call_async` root);
//! 3. binds the context to the thread pointer in the coroutine-local store;
//! 4. drives the coroutine via `Thread::into_async` on the worker `LocalSet`;
//! 5. interprets `ngx.exit` (sentinel error with `exit_code` set) and builds
//!    the outgoing [`http::Response`].
//!
//! T5.2a scope: a single `content_by_lua` phase. The full openresty order
//! (`rewrite` -> `access` -> `content` -> `header_filter` -> `body_filter` ->
//! `log`) and `init`/`init_worker`/`balancer` hooks land in T5.2c.

use std::cell::RefCell;
use std::rc::Rc;

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode};
use mlua::{Function, Lua, Thread};

use crate::context::{self, RequestContext};
use crate::ngx::EXIT_SENTINEL;

/// A worker-wide Lua VM configured with one loaded phase function.
///
/// `!Send`: one VM per worker thread (Q12). Drive on a `tokio::task::LocalSet`.
pub struct Pipeline {
    lua: Lua,
    content_fn: Function,
}

/// Outcome of driving one request through the pipeline.
pub struct PhaseOutcome {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub said: bool,
}

impl Pipeline {
    /// Build a pipeline whose `content_by_lua` is the given Lua chunk.
    ///
    /// The chunk must evaluate to a **function** (the openresty convention:
    /// `return function() ... end` or a bare `function() ... end` chunk). The
    /// worker VM is constructed via [`crate::worker_vm`].
    pub fn new(content_by_lua: &str) -> mlua::Result<Self> {
        let lua = crate::worker_vm()?;
        let content_fn = lua.load(content_by_lua).eval::<Function>()?;
        Ok(Self { lua, content_fn })
    }

    /// Run one request through the `content` phase and return its outcome.
    ///
    /// The request body type `B` is unused (T5.2a reads only the head); body
    /// reading (`ngx.req.read_body`) lands with cosocket/T5.2b.
    pub async fn serve_request<B>(&self, req: Request<B>) -> PhaseOutcome {
        let (parts, _body) = req.into_parts();
        self.run_content(&parts).await
    }

    async fn run_content(&self, parts: &http::request::Parts) -> PhaseOutcome {
        let ctx = Rc::new(RefCell::new(RequestContext::from_parts(parts)));

        // Explicit thread so current_thread() resolves here (Q13).
        let thread = match self.lua.create_thread(self.content_fn.clone()) {
            Ok(t) => t,
            Err(_) => return server_error(ctx, "thread create"),
        };
        let ptr = thread.to_pointer() as usize;
        context::bind(&self.lua, ptr, ctx.clone());

        let result = drive_thread(&thread).await;

        context::unbind(&self.lua, ptr);

        match result {
            Ok(()) => {}
            Err(e) => {
                // ngx.exit raises the sentinel; exit_code disambiguates clean
                // exits from genuine Lua errors.
                let is_exit = ctx.borrow().exit_code.is_some()
                    || matches!(&e, mlua::Error::RuntimeError(m) if m == EXIT_SENTINEL);
                if !is_exit {
                    tracing_error(&e);
                    return server_error(ctx, "content_by_lua");
                }
            }
        }

        let g = ctx.borrow();
        PhaseOutcome {
            status: g.status,
            headers: g.resp_headers.clone(),
            body: g.body.clone(),
            said: g.said,
        }
    }
}

/// Drive a Lua coroutine thread to completion via `into_async`.
async fn drive_thread(thread: &Thread) -> mlua::Result<()> {
    let fut = thread.clone().into_async::<()>(())?;
    fut.await
}

/// Materialise a `server_error` (500) outcome from a partial context.
fn server_error(ctx: Rc<RefCell<RequestContext>>, what: &str) -> PhaseOutcome {
    let g = ctx.borrow();
    tracing::error!(target: "init-pro", what, status = g.status, "router phase failed");
    PhaseOutcome {
        status: 500,
        headers: g.resp_headers.clone(),
        body: g.body.clone(),
        said: g.said,
    }
}

fn tracing_error(e: &mlua::Error) {
    tracing::error!(target: "init-pro", error = %e, "content_by_lua raised");
}

/// Turn a [`PhaseOutcome`] into an [`http::Response`], applying the openresty
/// default `Content-Type: text/plain` when only `ngx.say`/`ngx.print` were used
/// and no content-type was set, plus `Content-Length`.
pub fn build_response(out: PhaseOutcome) -> Response<Bytes> {
    let mut builder = Response::builder().status(
        StatusCode::from_u16(out.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
    );
    let mut has_content_type = false;
    for (name, value) in &out.headers {
        if name == "content-type" {
            has_content_type = true;
        }
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(n, v);
        }
    }
    if out.said && !has_content_type && !out.body.is_empty() {
        builder = builder.header(http::header::CONTENT_TYPE, "text/plain");
    }
    let body = Bytes::from(out.body);
    builder = builder.header(http::header::CONTENT_LENGTH, body.len());
    builder.body(body).unwrap_or_else(|_| {
        Response::new(Bytes::new())
    })
}
