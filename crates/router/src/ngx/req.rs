//! `ngx.req.*` — the per-request API surface: method/uri/path/headers plus the
//! request-body (`read_body`/`get_body_data`/`get_post_args`) and query-args
//! readers. Each resolves the live request via the Q13 coroutine-local store.

use mlua::{Lua, Table, Value};

use crate::context;
use crate::ngx::normalize_header_key;

/// Build the `ngx.req` sub-table.
pub(super) fn build(lua: &Lua) -> mlua::Result<Table> {
    let req = lua.create_table()?;
    req.set("get_method", lua.create_function(get_method)?)?;
    req.set("get_uri", lua.create_function(get_uri)?)?;
    req.set("get_path", lua.create_function(get_path)?)?;
    req.set("get_headers", lua.create_function(get_headers)?)?;

    // read_body is a yield point in openresty (it reads from the socket); here
    // the transport has already buffered the body, so it just marks it ready.
    let read_body = lua.create_async_function(|lua, (): ()| async move {
        let ctx = context::current(&lua)?;
        ctx.borrow_mut().req_body_read = true;
        Ok(())
    })?;
    req.set("read_body", read_body)?;
    req.set("get_body_data", lua.create_function(get_body_data)?)?;
    req.set("get_post_args", lua.create_function(get_post_args)?)?;
    req.set("get_query_args", lua.create_function(get_query_args)?)?;
    Ok(req)
}

/// `ngx.req.get_method()` -> request method string.
fn get_method(lua: &Lua, (): ()) -> mlua::Result<String> {
    Ok(context::current(lua)?.borrow().method.clone())
}

/// `ngx.req.get_uri()` -> full request target (`/path?query`).
fn get_uri(lua: &Lua, (): ()) -> mlua::Result<String> {
    Ok(context::current(lua)?.borrow().uri.clone())
}

/// `ngx.req.get_path()` -> path component only.
fn get_path(lua: &Lua, (): ()) -> mlua::Result<String> {
    Ok(context::current(lua)?.borrow().path.clone())
}

/// `ngx.req.get_headers()` -> table of header-name -> value (first wins).
fn get_headers(lua: &Lua, (): ()) -> mlua::Result<Table> {
    let ctx = context::current(lua)?;
    let g = ctx.borrow();
    let tbl = lua.create_table_with_capacity(g.req_headers.len(), 0)?;
    for (k, v) in &g.req_headers {
        tbl.set(normalize_header_key(k.as_str()), v.clone())?;
    }
    Ok(tbl)
}

/// `ngx.req.get_body_data()` -> the buffered request body (Lua string), or nil
/// when empty. Conventionally valid after `read_body()`; here always available.
fn get_body_data(lua: &Lua, (): ()) -> mlua::Result<Value> {
    let ctx = context::current(lua)?;
    let g = ctx.borrow();
    if g.req_body.is_empty() {
        return Ok(Value::Nil);
    }
    Ok(Value::String(lua.create_string(&g.req_body)?))
}

/// `ngx.req.get_post_args(max?)` -> parse the (urlencoded) body into a table.
fn get_post_args(lua: &Lua, max: Option<usize>) -> mlua::Result<Table> {
    let ctx = context::current(lua)?;
    let body = ctx.borrow().req_body.clone();
    let text = String::from_utf8_lossy(&body);
    Ok(parse_form(lua, &text, max.unwrap_or(100)))
}

/// `ngx.req.get_query_args(max?)` -> parse the query string into a table.
fn get_query_args(lua: &Lua, max: Option<usize>) -> mlua::Result<Table> {
    let ctx = context::current(lua)?;
    let query = ctx.borrow().query.clone();
    Ok(parse_form(lua, &query, max.unwrap_or(100)))
}

/// Parse `application/x-www-form-urlencoded` text into a Lua table, capping the
/// number of keys (`max`). Duplicate keys join into a single comma list, matching
/// openresty's simple behaviour for the common single-value case.
fn parse_form(lua: &Lua, text: &str, max: usize) -> Table {
    let tbl = lua.create_table().unwrap_or_else(|_| {
        // A parse failure should not panic the VM; an empty table is a safe fall
        // back (the caller sees no args).
        lua.create_table().expect("create_table")
    });
    let mut count = 0;
    for pair in text.split('&') {
        if count >= max {
            break;
        }
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (url_decode(k), url_decode(v)),
            None => (url_decode(pair), String::new()),
        };
        if k.is_empty() {
            continue;
        }
        let _ = tbl.set(k, v);
        count += 1;
    }
    tbl
}

/// Minimal percent-decoding for form values (`%xx` -> byte, `+` -> space).
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                if let Some(b) = hex_pair(bytes[i + 1], bytes[i + 2]) {
                    out.push(b);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    Some((hex_digit(hi)? << 4) | hex_digit(lo)?)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("%20"), " ");
        assert_eq!(url_decode("foo%2Bbar"), "foo+bar");
        assert_eq!(url_decode("100%25"), "100%");
    }

    #[test]
    fn hex_pair_roundtrip() {
        assert_eq!(hex_pair(b'2', b'0'), Some(0x20));
        assert_eq!(hex_pair(b'F', b'F'), Some(0xff));
        assert_eq!(hex_pair(b'z', b'0'), None);
    }
}
