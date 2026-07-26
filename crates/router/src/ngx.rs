//! The `ngx.*` global surface for the Router VM.
//!
//! # What lives here
//! - `ngx.sleep(ms)` — the coroutine<->async bridge primitive (T5.1).
//! - Per-request API, resolved to the **current** coroutine's context via the
//!   Q13 coroutine-local binding ([`crate::context::current`]):
//!   - `ngx.status` (get/set), `ngx.header[NAME] = VAL` (header proxy),
//!   - `ngx.req.get_method()` / `get_uri()` / `get_headers()`,
//!   - `ngx.say(...)` / `ngx.print(...)`, `ngx.exit(code)`.
//!
//! # Why a metatable on `ngx`
//! `ngx.status = 201` and `ngx.header["X"] = "Y"` must write to the *current
//! request*, but `ngx` is a single global table shared by all coroutines.
//! Intercepting assignment requires `__newindex`; intercepting reads of the
//! magic keys requires `__index`. Non-magic keys (the real functions like
//! `sleep`, `say`, `req`) fall through to the table itself via `rawget`/`rawset`.

use std::time::Duration;

use mlua::{Lua, LuaString, Table, Value};

use crate::context;

/// Sentinel raised by [`exit`] to unwind the Lua stack (openresty's `ngx.exit`
/// never returns). The pipeline recognises it via `exit_code` being set.
pub(crate) const EXIT_SENTINEL: &str = "\u{0}ngx.exit\u{0}";

/// Register the `ngx` global table on a worker VM, including the coroutine-local
/// [`context::ContextStore`].
pub fn register(lua: &Lua) -> mlua::Result<()> {
    context::install_store(lua);

    let ngx = lua.create_table()?;

    // --- async bridge primitive (T5.1) ---
    let sleep = lua.create_async_function(|_lua, ms: u64| async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(())
    })?;
    ngx.raw_set("sleep", sleep)?;

    // --- per-request functions ---
    ngx.raw_set("say", lua.create_function(say)?)?;
    ngx.raw_set("print", lua.create_function(print_body)?)?;
    ngx.raw_set("exit", lua.create_function(exit)?)?;

    // --- ngx.req sub-table ---
    let req = lua.create_table()?;
    req.set("get_method", lua.create_function(req_get_method)?)?;
    req.set("get_uri", lua.create_function(req_get_uri)?)?;
    req.set("get_path", lua.create_function(req_get_path)?)?;
    req.set("get_headers", lua.create_function(req_get_headers)?)?;
    ngx.raw_set("req", req)?;

    // --- ngx.socket.tcp (cosocket, T5.2b) ---
    let socket = lua.create_table()?;
    socket.set(
        "tcp",
        lua.create_function(|lua, (): ()| {
            lua.create_userdata(crate::cosocket::Cosocket::new())
        })?,
    )?;
    ngx.raw_set("socket", socket)?;

    // --- intercept `status` / `header` reads & writes via a metatable ---
    let mt = lua.create_table()?;
    mt.set("__index", lua.create_function(index_get)?)?;
    mt.set("__newindex", lua.create_function(index_set)?)?;
    ngx.set_metatable(Some(mt))?;

    lua.globals().set("ngx", ngx)?;
    Ok(())
}

/// `__index`: route `status`/`header` to the live request; else rawget.
fn index_get(_lua: &Lua, (tbl, key): (Table, LuaString)) -> mlua::Result<Value> {
    match &*key.to_str()? {
        "status" => {
            let ctx = context::current(_lua)?;
            let status = ctx.borrow().status;
            Ok(Value::Integer(status as i64))
        }
        "header" => {
            let ctx = context::current(_lua)?;
            Ok(Value::Table(header_proxy(_lua, ctx)?))
        }
        other => tbl.raw_get::<Value>(other),
    }
}

/// `__newindex`: route `status`/`header` writes to the live request; else rawset.
fn index_set(_lua: &Lua, (tbl, key, value): (Table, LuaString, Value)) -> mlua::Result<()> {
    match &*key.to_str()? {
        "status" => {
            let n = coerce_u16(&value, "ngx.status")?;
            context::current(_lua)?.borrow_mut().status = n;
            Ok(())
        }
        "header" => {
            // `ngx.header = {...}` wholesale set from a table.
            let ctx = context::current(_lua)?;
            let Value::Table(t) = value else {
                return Err(mlua::Error::RuntimeError(
                    "ngx.header must be set to a table".to_owned(),
                ));
            };
            let mut hdrs = ctx.borrow_mut();
            hdrs.resp_headers.clear();
            for pair in t.pairs::<LuaString, Value>() {
                let (k, v) = pair?;
                hdrs.resp_headers.push((normalize_header(&k), value_to_string(v)?));
            }
            Ok(())
        }
        other => tbl.raw_set(other, value),
    }
}

/// `ngx.say(s, ...)` — append args + `\n`; mark default `text/plain`.
fn say(lua: &Lua, args: mlua::MultiValue) -> mlua::Result<()> {
    let ctx = context::current(lua)?;
    let mut b = ctx.borrow_mut();
    append_args(&mut b.body, args)?;
    b.body.push(b'\n');
    b.said = true;
    Ok(())
}

/// `ngx.print(s, ...)` — append args with no trailing newline.
fn print_body(lua: &Lua, args: mlua::MultiValue) -> mlua::Result<()> {
    let ctx = context::current(lua)?;
    let mut b = ctx.borrow_mut();
    append_args(&mut b.body, args)?;
    Ok(())
}

/// `ngx.exit(code)` — record the status and unwind via the sentinel error.
fn exit(lua: &Lua, code: i32) -> mlua::Result<()> {
    let ctx = context::current(lua)?;
    {
        let mut g = ctx.borrow_mut();
        if g.status == 200 {
            g.status = code.clamp(0, 999) as u16;
        }
        g.exit_code = Some(code);
    }
    Err(mlua::Error::RuntimeError(EXIT_SENTINEL.to_owned()))
}

/// `ngx.req.get_method()` -> request method string.
fn req_get_method(lua: &Lua, (): ()) -> mlua::Result<String> {
    Ok(context::current(lua)?.borrow().method.clone())
}

/// `ngx.req.get_uri()` -> full request target (`/path?query`).
fn req_get_uri(lua: &Lua, (): ()) -> mlua::Result<String> {
    Ok(context::current(lua)?.borrow().uri.clone())
}

/// `ngx.req.get_path()` -> path component only.
fn req_get_path(lua: &Lua, (): ()) -> mlua::Result<String> {
    Ok(context::current(lua)?.borrow().path.clone())
}

/// `ngx.req.get_headers()` -> table of header-name -> value (first wins).
fn req_get_headers(lua: &Lua, (): ()) -> mlua::Result<Table> {
    let ctx = context::current(lua)?;
    let g = ctx.borrow();
    let tbl = lua.create_table_with_capacity(g.req_headers.len(), 0)?;
    for (k, v) in &g.req_headers {
        tbl.set(normalize_header_key(k.as_str()), v.clone())?;
    }
    Ok(tbl)
}

// ---- helpers ----

/// Build a per-request header proxy: `ngx.header["Content-Type"] = "..."`.
///
/// Reads/writes route through the metatable to the live request's
/// `resp_headers`, normalising names (`Content_Type` -> `content-type`) the way
/// openresty maps dotted/underscored header names.
fn header_proxy(lua: &Lua, ctx: std::rc::Rc<std::cell::RefCell<context::RequestContext>>) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let mt = lua.create_table()?;
    // Capture a cheap clone of the Rc handle; no guard is held across an await
    // (these closures are synchronous).
    let ctx_get = ctx.clone();
    let ctx_set = ctx.clone();
    mt.set(
        "__index",
        lua.create_function(move |lua, (_self, key): (Table, LuaString)| {
            let key = normalize_header(&key);
            let val = ctx_get
                .borrow()
                .resp_headers
                .iter()
                .find(|(n, _)| *n == key)
                .map(|(_, v)| v.clone());
            match val {
                Some(v) => Ok(Value::String(lua.create_string(&v)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    mt.set(
        "__newindex",
        lua.create_function(move |_lua, (_self, key, value): (Table, LuaString, Value)| {
            let key = normalize_header(&key);
            let mut g = ctx_set.borrow_mut();
            match value {
                Value::Nil => g.resp_headers.retain(|(n, _)| *n != key),
                v => {
                    if let Some(slot) = g.resp_headers.iter_mut().find(|(n, _)| *n == key) {
                        slot.1 = value_to_string(v)?;
                    } else {
                        g.resp_headers.push((key, value_to_string(v)?));
                    }
                }
            }
            Ok(())
        })?,
    )?;
    proxy.set_metatable(Some(mt))?;
    Ok(proxy)
}

/// Append the variadic Lua args to `out`, coerced to strings (openresty `tostring` semantics).
fn append_args(out: &mut Vec<u8>, args: mlua::MultiValue) -> mlua::Result<()> {
    // openresty concatenates the arguments with NO separator (unlike Lua's
    // global `print`, which joins with tabs).
    for v in args {
        out.extend_from_slice(value_to_string(v)?.as_bytes());
    }
    Ok(())
}

/// Coerce a Lua value to a Rust string (numbers, booleans, nil, strings).
fn value_to_string(v: Value) -> mlua::Result<String> {
    Ok(match v {
        Value::String(s) => s.to_str()?.to_owned(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => "nil".to_owned(),
        other => format!("{:?}", other),
    })
}

/// `ngx.status = x` accepts a number and clamps into the HTTP range.
fn coerce_u16(v: &Value, what: &str) -> mlua::Result<u16> {
    match v {
        Value::Integer(i) => Ok((*i).clamp(100, 599) as u16),
        Value::Number(n) => Ok((*n as i64).clamp(100, 599) as u16),
        _ => Err(mlua::Error::RuntimeError(format!(
            "{what} must be a number"
        ))),
    }
}

/// Normalise a header name for storage/comparison: lowercase, `_` -> `-`.
fn normalize_header(key: &LuaString) -> String {
    normalize_header_key(&key.to_string_lossy())
}

fn normalize_header_key(s: &str) -> String {
    s.to_ascii_lowercase().replace('_', "-")
}
