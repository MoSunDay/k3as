//! `ngx.header[...]` — the response-header proxy. Reads/writes route through a
//! metatable to the live request's `resp_headers`, normalising names
//! (`Content_Type` -> `content-type`) the way openresty maps dotted/underscored
//! header names.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, LuaString, Table, Value};

use crate::context::{self, RequestContext};
use crate::ngx::{normalize_header, value_to_string};

/// Build a per-request header proxy: `ngx.header["Content-Type"] = "..."`.
///
/// Captures an [`Rc`] clone of the context; no guard is held across an await
/// (these closures are synchronous).
pub(super) fn proxy(
    lua: &Lua,
    ctx: Rc<RefCell<RequestContext>>,
) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let mt = lua.create_table()?;
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
            set_one(&ctx_set, normalize_header(&key), value)
        })?,
    )?;
    proxy.set_metatable(Some(mt))?;
    Ok(proxy)
}

/// `ngx.header = {table}` — wholesale replace the response headers from a table.
pub(super) fn bulk_set(lua: &Lua, value: Value) -> mlua::Result<()> {
    let ctx = context::current(lua)?;
    let Value::Table(t) = value else {
        return Err(mlua::Error::RuntimeError(
            "ngx.header must be set to a table".to_owned(),
        ));
    };
    let mut hdrs = ctx.borrow_mut();
    hdrs.resp_headers.clear();
    for pair in t.pairs::<LuaString, Value>() {
        let (k, v) = pair?;
        hdrs.resp_headers
            .push((normalize_header(&k), value_to_string(v)?));
    }
    Ok(())
}

/// Write one header value into the live request, honouring the openresty
/// multi-value (table) and delete (nil) semantics.
fn set_one(
    ctx: &Rc<RefCell<RequestContext>>,
    key: String,
    value: Value,
) -> mlua::Result<()> {
    let mut g = ctx.borrow_mut();
    match value {
        Value::Nil => g.resp_headers.retain(|(n, _)| *n != key),
        Value::Table(t) => {
            // Replace all existing values for this header with the table's values.
            g.resp_headers.retain(|(n, _)| *n != key);
            for pair in t.pairs::<LuaString, Value>() {
                let (_k, v) = pair?;
                g.resp_headers.push((key.clone(), value_to_string(v)?));
            }
        }
        v => {
            g.resp_headers.retain(|(n, _)| *n != key);
            g.resp_headers.push((key, value_to_string(v)?));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalize_is_lowercase_dashes() {
        assert_eq!(crate::ngx::normalize_header_key("Content_Type"), "content-type");
        assert_eq!(crate::ngx::normalize_header_key("X-Served-By"), "x-served-by");
    }
}
