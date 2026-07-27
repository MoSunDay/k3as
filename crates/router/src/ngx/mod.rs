//! The `ngx.*` global surface for the Router VM.
//!
//! Assembled from focused submodules:
//! - [`super::output`] — `sleep`/`say`/`print`/`exit` + time helpers,
//! - [`super::req`] — the `ngx.req` request APIs (method/uri/headers/body),
//! - [`super::header`] — the `ngx.header[...]` response-header proxy,
//! - [`super::var`] — `ngx.var.*` request variables, `ngx.exec`/`ngx.redirect`.
//!
//! # Why a metatable on `ngx`
//! `ngx.status = 201`, `ngx.header["X"] = "Y"`, `ngx.var.foo = "1"` and
//! `ngx.arg[1] = ...` must touch the *current* request, but `ngx` is a single
//! global table shared by all coroutines. Intercepting these keys requires
//! `__newindex`; intercepting reads requires `__index`. Non-magic keys (the real
//! functions like `sleep`, `say`, `req`, `exec`) fall through to the table
//! itself via `rawget`/`rawset`.

use mlua::{Lua, LuaString, Table, Value};

use crate::context;

mod header;
mod output;
mod req;
mod var;

/// Sentinel raised by [`crate::ngx::output::exit`] to unwind the Lua stack
/// (openresty's `ngx.exit` never returns). The pipeline recognises it via
/// `exit_code` being set.
pub(crate) const EXIT_SENTINEL: &str = "\u{0}ngx.exit\u{0}";

/// Register the `ngx` global table on a worker VM, including the coroutine-local
/// [`context::ContextStore`] and every submodule surface.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    context::install_store(lua);

    let ngx = lua.create_table()?;

    // --- async bridge primitive + response output + time ---
    output::install(lua, &ngx)?;
    // --- ngx.req sub-table ---
    ngx.raw_set("req", req::build(lua)?)?;
    // --- control flow: ngx.exec / ngx.redirect ---
    var::install_control(lua, &ngx)?;
    // --- ngx.socket.tcp (cosocket, T5.2b) ---
    let socket = lua.create_table()?;
    socket.set(
        "tcp",
        lua.create_function(|lua, (): ()| lua.create_userdata(crate::cosocket::Cosocket::new()))?,
    )?;
    ngx.raw_set("socket", socket)?;

    // --- intercept magic field reads/writes via a metatable ---
    let mt = lua.create_table()?;
    mt.set("__index", lua.create_function(index_get)?)?;
    mt.set("__newindex", lua.create_function(index_set)?)?;
    ngx.set_metatable(Some(mt))?;

    lua.globals().set("ngx", ngx)?;
    Ok(())
}

/// `__index`: route magic keys to the live request; else rawget.
fn index_get(lua: &Lua, (tbl, key): (Table, LuaString)) -> mlua::Result<Value> {
    match &*key.to_str()? {
        "status" => {
            let ctx = context::current(lua)?;
            let status = ctx.borrow().status;
            Ok(Value::Integer(status as i64))
        }
        "header" => {
            let ctx = context::current(lua)?;
            Ok(Value::Table(header::proxy(lua, ctx)?))
        }
        "var" => {
            let ctx = context::current(lua)?;
            Ok(Value::Table(var::proxy(lua, ctx)?))
        }
        "arg" => {
            let ctx = context::current(lua)?;
            Ok(Value::Table(var::arg_proxy(lua, ctx)?))
        }
        other => tbl.raw_get::<Value>(other),
    }
}

/// `__newindex`: route magic-key writes to the live request; else rawset.
fn index_set(lua: &Lua, (tbl, key, value): (Table, LuaString, Value)) -> mlua::Result<()> {
    match &*key.to_str()? {
        "status" => {
            let n = coerce_u16(&value, "ngx.status")?;
            context::current(lua)?.borrow_mut().status = n;
            Ok(())
        }
        "header" => header::bulk_set(lua, value),
        "var" => var::bulk_set(lua, value),
        "arg" => Err(mlua::Error::RuntimeError(
            "ngx.arg is set by the pipeline; assign ngx.arg[1] instead".to_owned(),
        )),
        other => tbl.raw_set(other, value),
    }
}

// ---- shared coercion helpers (used across submodules) ----

/// Coerce a Lua value to a Rust string (numbers, booleans, nil, strings).
pub(super) fn value_to_string(v: Value) -> mlua::Result<String> {
    Ok(match v {
        Value::String(s) => s.to_str()?.to_owned(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => "nil".to_owned(),
        other => format!("{other:?}"),
    })
}

/// Accept a number and clamp it into the HTTP status range.
pub(super) fn coerce_u16(v: &Value, what: &str) -> mlua::Result<u16> {
    match v {
        Value::Integer(i) => Ok((*i).clamp(100, 599) as u16),
        Value::Number(n) => Ok((*n as i64).clamp(100, 599) as u16),
        _ => Err(mlua::Error::RuntimeError(format!(
            "{what} must be a number"
        ))),
    }
}

/// Normalise a header name for storage/comparison: lowercase, `_` -> `-`.
pub(super) fn normalize_header(key: &LuaString) -> String {
    normalize_header_key(&key.to_string_lossy())
}

pub(super) fn normalize_header_key(s: &str) -> String {
    s.to_ascii_lowercase().replace('_', "-")
}

#[cfg(test)]
mod tests {
    /// `EXIT_SENTINEL` keeps its null-byte framing so it can never collide with a
    /// user-raised RuntimeError string.
    #[test]
    fn exit_sentinel_is_null_framed() {
        assert!(super::EXIT_SENTINEL.starts_with('\u{0}'));
        assert!(super::EXIT_SENTINEL.ends_with('\u{0}'));
    }
}
