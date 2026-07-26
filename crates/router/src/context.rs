//! Per-request context and the **coroutine-local binding store** (ADR **Q13**).
//!
//! The worker VM is shared across all requests (ADR Q12), and per-request Lua
//! coroutines **interleave** at `await` points (the proven T5.1 model). So the
//! `ngx.*` surface must resolve the *currently-running request* per coroutine —
//! not per VM, not per thread. A plain global swap races the instant coroutine
//! A parks and coroutine B runs (Q13 spike: app_data-swap is unsafe).
//!
//! The chosen mechanism: each request phase runs as an **explicit** Lua `Thread`
//! (created via `Lua::create_thread`, driven via `Thread::into_async` — *not*
//! the implicit coroutines `call_async` spawns, which collapse to the root
//! thread). We key the live [`RequestContext`] by the coroutine's stable
//! pointer (`Thread::to_pointer()`). A registered function reads
//! `Lua::current_thread().to_pointer()` and resolves its own request —
//! distinct and stable even across interleaving (Q13 spike PASS).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use http::request::Parts;
use mlua::Lua;

/// Active per-request state, owned by the pipeline and mutated from Lua.
///
/// Single-threaded (`!Send`) by design: one VM per worker thread (Q12), driven
/// on a `tokio::task::LocalSet`. Interior mutability via the enclosing
/// [`Rc`]`<`[`RefCell`]`<..>>` lets the Lua `__index`/`__newindex` closures write
/// status / headers / body without an `&mut` borrow of the VM.
pub struct RequestContext {
    /// Incoming request method (`GET`, `POST`, ...).
    pub method: String,
    /// Raw request target as sent on the wire (`/path?query`).
    pub uri: String,
    /// Path component of [`Self::uri`] (no query).
    pub path: String,
    /// Query string component of [`Self::uri`] (the part after `?`).
    pub query: String,
    /// Incoming headers, `(name, value)` in received order.
    pub req_headers: Vec<(String, String)>,
    /// Buffered request body bytes (read by the transport before the phase).
    pub req_body: Vec<u8>,
    /// Whether `ngx.req.read_body()` has marked the body as available.
    pub req_body_read: bool,

    /// Status code the Lua phase wrote (default `200`).
    pub status: u16,
    /// Outgoing headers accumulated by `ngx.header[...] = ...`.
    pub resp_headers: Vec<(String, String)>,
    /// Body bytes accumulated by `ngx.say` / `ngx.print`.
    pub body: Vec<u8>,
    /// `ngx.exit(code)` sentinel: when `Some`, the generative phases must stop.
    pub exit_code: Option<i32>,
    /// Whether `ngx.say` was used (drives the default `Content-Type`).
    pub said: bool,

    /// User vars set via `ngx.var.NAME = value` (openresty `$NAME`).
    pub user_vars: Vec<(String, String)>,
    /// `ngx.arg[1]` carrier for `body_filter_by_lua` (the current chunk).
    pub arg_body: Vec<u8>,
    /// `ngx.arg[2]` carrier for `body_filter_by_lua` (is-final-chunk flag).
    pub arg_eof: bool,
    /// `ngx.exec(uri)` re-dispatch target; the pipeline honours it once set.
    pub exec_uri: Option<String>,
}

impl RequestContext {
    /// Build the context from the parsed [`http::Request`] head (empty body).
    pub fn from_parts(parts: &Parts) -> Self {
        let method = parts.method.as_str().to_owned();
        let uri = parts.uri.to_string();
        let path = parts.uri.path().to_owned();
        let query = parts.uri.query().unwrap_or("").to_owned();
        let req_headers = parts
            .headers
            .iter()
            .map(|(name, val)| {
                (
                    name.as_str().to_owned(),
                    val.to_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        Self {
            method,
            uri,
            path,
            query,
            req_headers,
            req_body: Vec::new(),
            req_body_read: false,
            status: 200,
            resp_headers: Vec::new(),
            body: Vec::new(),
            exit_code: None,
            said: false,
            user_vars: Vec::new(),
            arg_body: Vec::new(),
            arg_eof: false,
            exec_uri: None,
        }
    }

    /// Set the buffered request body (called by the data plane after reading).
    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.req_body = body;
        self
    }

    /// Reset the *generative* response state for an `ngx.exec` re-dispatch:
    /// a fresh status/body, as if the request just arrived at the new URI.
    pub fn reset_for_exec(&mut self, new_uri: String) {
        self.uri = new_uri.clone();
        let (path, query) = match new_uri.split_once('?') {
            Some((p, q)) => (p.to_owned(), q.to_owned()),
            None => (new_uri, String::new()),
        };
        self.path = path;
        self.query = query;
        self.status = 200;
        self.body.clear();
        self.said = false;
        self.exit_code = None;
        self.exec_uri = None;
    }

    /// The request scheme: `https` when TLS lands (T5.4), else `http`.
    pub fn scheme(&self) -> &'static str {
        "http"
    }

    /// The `Host` header value (first one wins), or empty.
    pub fn host(&self) -> String {
        self.req_headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

/// VM-global, coroutine-keyed map of live request contexts.
///
/// Stored once in the VM's app-data. Keys are `Thread::to_pointer()` values —
/// stable for the lifetime of a coroutine and distinct per coroutine (Q13).
pub struct ContextStore(RefCell<HashMap<usize, Rc<RefCell<RequestContext>>>>);

impl ContextStore {
    pub fn new() -> Self {
        Self(RefCell::new(HashMap::new()))
    }
}

/// Install a fresh, empty app-data [`ContextStore`] on a VM.
pub fn install_store(lua: &Lua) {
    lua.set_app_data(ContextStore::new());
}

/// Bind a [`RequestContext`] to a coroutine, keyed by its thread pointer.
pub fn bind(lua: &Lua, thread_ptr: usize, ctx: Rc<RefCell<RequestContext>>) {
    let store = lua
        .app_data_ref::<ContextStore>()
        .expect("ContextStore installed on the VM");
    store.0.borrow_mut().insert(thread_ptr, ctx);
}

/// Remove a coroutine's binding (after the phase future completes).
pub fn unbind(lua: &Lua, thread_ptr: usize) {
    if let Some(store) = lua.app_data_ref::<ContextStore>() {
        store.0.borrow_mut().remove(&thread_ptr);
    }
}

/// Resolve the [`RequestContext`] for the **currently running** coroutine.
///
/// Errors when called outside a bound request phase (e.g. at VM init). Returns
/// a cloned [`Rc`] handle so no app-data / RefCell guard is held across an
/// `await` (a held guard would either deadlock the RefCell or be held across a
/// yield — both illegal in the bridge model).
pub fn current(lua: &Lua) -> mlua::Result<Rc<RefCell<RequestContext>>> {
    let ptr = lua.current_thread().to_pointer() as usize;
    let rc = lua
        .app_data_ref::<ContextStore>()
        .and_then(|store| store.0.borrow().get(&ptr).cloned())
        .ok_or_else(|| {
            mlua::Error::RuntimeError(
                "ngx.* per-request API used outside a request phase".to_owned(),
            )
        })?;
    Ok(rc)
}
