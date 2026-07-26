//! `ngx.var.*` request variables, the `ngx.arg[1/2]` body-filter carrier, and
//! the control-flow primitives `ngx.exec` (internal redirect) / `ngx.redirect`
//! (external redirect). All resolve the live request via the Q13 store.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, LuaString, Table, Value};

use crate::context::{self, RequestContext};
use crate::ngx::{value_to_string, EXIT_SENTINEL};

/// Install `ngx.exec` / `ngx.redirect` on the `ngx` table.
pub(super) fn install_control(lua: &Lua, ngx: &Table) -> mlua::Result<()> {
    ngx.raw_set("exec", lua.create_function(exec)?)?;
    ngx.raw_set("redirect", lua.create_function(redirect)?)?;
    Ok(())
}

/// Build the `ngx.var` proxy: reads of the openresty-essential `$NAME` set are
/// computed live; unknown names fall back to user vars; writes set user vars.
pub(super) fn proxy(lua: &Lua, ctx: Rc<RefCell<RequestContext>>) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let mt = lua.create_table()?;
    let ctx_get = ctx.clone();
    let ctx_set = ctx.clone();
    mt.set(
        "__index",
        lua.create_function(move |lua, (_self, key): (Table, LuaString)| {
            let key = key.to_string_lossy();
            let g = ctx_get.borrow();
            let val = match key.as_str() {
                "uri" => Some(g.path.clone()),
                "request_uri" => Some(g.uri.clone()),
                "args" | "query_string" => Some(g.query.clone()),
                "request_method" => Some(g.method.clone()),
                "scheme" => Some(g.scheme().to_owned()),
                "host" => Some(g.host()),
                _ => g
                    .user_vars
                    .iter()
                    .rev()
                    .find(|(n, _)| *n == key)
                    .map(|(_, v)| v.clone()),
            };
            match val {
                Some(v) => Ok(Value::String(lua.create_string(&v)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    mt.set(
        "__newindex",
        lua.create_function(move |_lua, (_self, key, value): (Table, LuaString, Value)| {
            let key = key.to_string_lossy();
            let mut g = ctx_set.borrow_mut();
            match value {
                Value::Nil => g.user_vars.retain(|(n, _)| *n != key),
                v => {
                    g.user_vars.retain(|(n, _)| *n != key);
                    g.user_vars.push((key, value_to_string(v)?));
                }
            }
            Ok(())
        })?,
    )?;
    proxy.set_metatable(Some(mt))?;
    Ok(proxy)
}

/// `ngx.var = {table}` — wholesale replace user vars from a table.
pub(super) fn bulk_set(lua: &Lua, value: Value) -> mlua::Result<()> {
    let ctx = context::current(lua)?;
    let Value::Table(t) = value else {
        return Err(mlua::Error::RuntimeError(
            "ngx.var must be set to a table".to_owned(),
        ));
    };
    let mut g = ctx.borrow_mut();
    g.user_vars.clear();
    for pair in t.pairs::<LuaString, Value>() {
        let (k, v) = pair?;
        g.user_vars
            .push((k.to_string_lossy(), value_to_string(v)?));
    }
    Ok(())
}

/// Build the `ngx.arg` proxy for `body_filter_by_lua`: `ngx.arg[1]` is the body
/// chunk, `ngx.arg[2]` is the is-final-chunk flag.
pub(super) fn arg_proxy(lua: &Lua, ctx: Rc<RefCell<RequestContext>>) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let mt = lua.create_table()?;
    let ctx_get = ctx.clone();
    let ctx_set = ctx.clone();
    mt.set(
        "__index",
        lua.create_function(move |lua, (_self, key): (Table, Value)| {
            let g = ctx_get.borrow();
            match key {
                Value::Integer(1) | Value::Number(_) => {
                    Ok(Value::String(lua.create_string(&g.arg_body)?))
                }
                Value::Integer(2) => Ok(Value::Boolean(g.arg_eof)),
                _ => Ok(Value::Nil),
            }
        })?,
    )?;
    mt.set(
        "__newindex",
        lua.create_function(move |_lua, (_self, key, value): (Table, Value, Value)| {
            let mut g = ctx_set.borrow_mut();
            match key {
                Value::Integer(1) | Value::Number(_) => {
                    g.arg_body = value_to_string(value)?.into_bytes();
                }
                Value::Integer(2) => {
                    if let Value::Boolean(b) = value {
                        g.arg_eof = b;
                    }
                }
                _ => {}
            }
            Ok(())
        })?,
    )?;
    proxy.set_metatable(Some(mt))?;
    Ok(proxy)
}

/// `ngx.exec(uri, args?)` — internal redirect: record the new target and unwind
/// the phase. The pipeline re-runs the generative phases for the new URI
/// (simplified vs openresty's route-table lookup, which lands with T5.4).
fn exec(lua: &Lua, (uri, args): (String, Option<String>)) -> mlua::Result<()> {
    let ctx = context::current(lua)?;
    {
        let mut g = ctx.borrow_mut();
        let target = match args {
            Some(ref a) if !a.is_empty() => format!("{uri}?{a}"),
            _ => uri,
        };
        g.exec_uri = Some(target);
        g.exit_code = Some(0);
    }
    Err(mlua::Error::RuntimeError(EXIT_SENTINEL.to_owned()))
}

/// `ngx.redirect(url, status?)` — emit a 3xx with `Location` and terminate.
fn redirect(lua: &Lua, (url, status): (String, Option<i32>)) -> mlua::Result<()> {
    let ctx = context::current(lua)?;
    let code = status.unwrap_or(302).clamp(301, 302) as u16;
    {
        let mut g = ctx.borrow_mut();
        g.status = code;
        g.resp_headers.retain(|(n, _)| *n != "location");
        g.resp_headers.push(("location".to_owned(), url));
        g.exit_code = Some(code as i32);
    }
    Err(mlua::Error::RuntimeError(EXIT_SENTINEL.to_owned()))
}
