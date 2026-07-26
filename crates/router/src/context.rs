//! Per-request context and the **coroutine-local binding store** (ADR **Q13**).
//!
//! The worker VM is shared across all requests (ADR Q12), and per-request Lua
//! coroutines **interleave** at `await` points (the proven T5.1 model). So the
//! `ngx.*` surface must resolve the *currently-running request* per coroutine —
//! not per VM, not per thread. A plain global swap races the instant coroutine
//! A parks and coroutine B runs (Q13 spike: app_data-swap is unsafe).
//!
//! The chosen mechanism: each request runs as an **explicit** Lua `Thread`
//! (created via `Lua::create_thread`, driven via `Thread::into_async` — *not*
//! the implicit coroutines `call_async` spawns, which collapse to the root
//! thread). We key the live [`RequestContext`] by the coroutine's stable
//! pointer (`Thread::to_pointer()`). A registered async function reads
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
/// [`Rc`]`<`[`RefCell`]`<..>> lets the Lua `__index`/`__newindex` closures write
/// status / headers / body without an `&mut` borrow of the VM.
pub struct RequestContext {
    /// Incoming request method (`GET`, `POST`, ...).
    pub method: String,
    /// Raw request target as sent on the wire (`/path?query`).
    pub uri: String,
    /// Path component of [`Self::uri`] (no query).
    pub path: String,
    /// Incoming headers, `(name, value)` in received order.
    pub req_headers: Vec<(String, String)>,

    /// Status code the Lua phase wrote (default `200`).
    pub status: u16,
    /// Outgoing headers accumulated by `ngx.header[...] = ...`.
    pub resp_headers: Vec<(String, String)>,
    /// Body bytes accumulated by `ngx.say` / `ngx.print`.
    pub body: Vec<u8>,
    /// `ngx.exit(code)` sentinel: when `Some`, the phase must terminate.
    pub exit_code: Option<i32>,
    /// Whether `ngx.say` was used (drives the default `Content-Type`).
    pub said: bool,
}

impl RequestContext {
    /// Build the context from the parsed [`http::Request`] head.
    pub fn from_parts(parts: &Parts) -> Self {
        let method = parts.method.as_str().to_owned();
        let uri = parts.uri.to_string();
        let path = parts
            .uri
            .path()
            .to_owned();
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
            req_headers,
            status: 200,
            resp_headers: Vec::new(),
            body: Vec::new(),
            exit_code: None,
            said: false,
        }
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
