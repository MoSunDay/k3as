//! `resty.string` (base64 + hex helpers) and the `resty.sha256` digest object.
//!
//! Scope A subset of lua-resty-string: `encode_base64`/`decode_base64`/`to_hex`/
//! `from_hex`, plus a `resty.sha256:new()`/`:update()`/`:final()` digest chain
//! backed by the `sha2` crate. `:final()` returns the lowercase-hex digest
//! (lua-resty-string returns binary + a separate `to_hex`; we collapse the two
//! steps for convenience — documented deviation).

use std::cell::RefCell;

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use mlua::{Lua, LuaString, Table, UserData, UserDataMethods};
use sha2::{Digest, Sha256};

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// Build the `resty.string` table.
pub(super) fn build_string(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("encode_base64", lua.create_function(encode_base64)?)?;
    t.set("decode_base64", lua.create_function(decode_base64)?)?;
    t.set("to_hex", lua.create_function(to_hex)?)?;
    t.set("from_hex", lua.create_function(from_hex)?)?;
    Ok(t)
}

/// Build the `resty.sha256` module-like table (`{ new = fn }`). Both
/// `resty.sha256.new()` and `resty.sha256:new()` work (self is accepted & ignored).
pub(super) fn build_sha256(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set(
        "new",
        lua.create_function(|lua, _: mlua::MultiValue| {
            lua.create_userdata(Sha256Digest(RefCell::new(Sha256::new())))
        })?,
    )?;
    Ok(t)
}

/// `resty.string.encode_base64(s, no_padding?)` -> standard base64 string.
fn encode_base64(
    _lua: &Lua,
    (s, no_pad): (LuaString, Option<bool>),
) -> mlua::Result<String> {
    let enc = if no_pad.unwrap_or(false) {
        STANDARD_NO_PAD.encode(s.as_bytes())
    } else {
        STANDARD.encode(s.as_bytes())
    };
    Ok(enc)
}

/// `resty.string.decode_base64(s)` -> decoded bytes; raises on bad input.
fn decode_base64(lua: &Lua, s: LuaString) -> mlua::Result<LuaString> {
    let bytes = STANDARD
        .decode(&s.as_bytes()[..])
        .map_err(|e| mlua::Error::RuntimeError(format!("decode_base64: {e}")))?;
    lua.create_string(&bytes)
}

/// `resty.string.to_hex(s)` -> lowercase hex string.
fn to_hex(_lua: &Lua, s: LuaString) -> mlua::Result<String> {
    Ok(hex_encode(&s.as_bytes()[..]))
}

/// `resty.string.from_hex(s)` -> decoded bytes; raises on odd length/bad char.
fn from_hex(lua: &Lua, s: LuaString) -> mlua::Result<LuaString> {
    let bytes = hex_decode(&s.as_bytes()[..])
        .map_err(|e| mlua::Error::RuntimeError(format!("from_hex: {e}")))?;
    lua.create_string(&bytes)
}

/// The `resty.sha256` digest object.
pub struct Sha256Digest(RefCell<Sha256>);

impl UserData for Sha256Digest {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("update", |_lua, this, data: LuaString| {
            this.0.borrow_mut().update(data.as_bytes());
            Ok(())
        });
        methods.add_method("final", |_lua, this, (): ()| {
            let out = this.0.borrow_mut().finalize_reset();
            Ok(hex_encode(out.as_slice()))
        });
        methods.add_method("reset", |_lua, this, (): ()| {
            this.0.borrow_mut().reset();
            Ok(())
        });
    }
}

/// Encode bytes as lowercase hex (no allocation beyond the string).
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX_LOWER[(b >> 4) as usize] as char);
        out.push(HEX_LOWER[(b & 0x0f) as usize] as char);
    }
    out
}

/// Decode a hex string; errors on odd length or non-hex bytes.
fn hex_decode(hex: &[u8]) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("odd length".to_owned());
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.chunks_exact(2) {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("invalid hex byte {b:#x}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_sha256_vector() {
        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(
            hex_encode(h.finalize().as_slice()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[test]
    fn empty_sha256() {
        assert_eq!(
            hex_encode(Sha256::new().finalize().as_slice()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
    #[test]
    fn hex_roundtrip() {
        let s = b"hello, world";
        assert_eq!(hex_decode(hex_encode(s).as_bytes()).unwrap(), s);
    }
    #[test]
    fn base64_known() {
        assert_eq!(STANDARD.encode(b"hello"), "aGVsbG8=");
    }
    #[test]
    fn from_hex_rejects_odd() {
        assert!(hex_decode(b"abc").is_err());
    }
}
