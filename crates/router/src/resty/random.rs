//! `resty.random` — CSPRNG primitives backed by `getrandom`.
//!
//! `bytes(n)` -> exactly n random bytes (Lua string). `token(n)` -> a URL-safe
//! base64 (no padding) token encoding n random bytes. Mirrors lua-resty-random's
//! surface (Scope A: no `pseudorandom`/`random_pseudo_bytes` aliases yet).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use mlua::{Lua, LuaString, Table};

/// Build the `resty.random` table.
pub(super) fn build(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("bytes", lua.create_function(bytes)?)?;
    t.set("token", lua.create_function(token)?)?;
    Ok(t)
}

/// `resty.random.bytes(n)` -> exactly n cryptographically-random bytes.
fn bytes(lua: &Lua, n: usize) -> mlua::Result<LuaString> {
    let mut buf = vec![0u8; n];
    fill_random(&mut buf, "bytes")?;
    lua.create_string(&buf)
}

/// `resty.random.token(n)` -> URL-safe base64 (no pad) of n random bytes.
fn token(_lua: &Lua, n: usize) -> mlua::Result<String> {
    let mut buf = vec![0u8; n];
    fill_random(&mut buf, "token")?;
    Ok(URL_SAFE_NO_PAD.encode(&buf))
}

/// Fill `buf` from the OS CSPRNG, mapping failures to a Lua runtime error.
fn fill_random(buf: &mut [u8], what: &str) -> mlua::Result<()> {
    getrandom::fill(buf).map_err(|e| mlua::Error::RuntimeError(format!("resty.random.{what}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fill_is_nonzero_and_full() {
        let mut a = [0u8; 32];
        fill_random(&mut a, "test").expect("fill");
        assert!(a.iter().any(|&b| b != 0));
        let mut b = [0u8; 32];
        fill_random(&mut b, "test").expect("fill");
        // Astronomically unlikely to collide.
        assert_ne!(a, b);
    }
    #[test]
    fn token_is_urlsafe_charset() {
        let t = token(&Lua::new(), 16).expect("token");
        assert!(t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
