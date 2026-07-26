//! The `resty.*` standard-library surface + the worker-wide `ngx.shared.DICT`
//! zone, for the Router VM (TODO **T5.3**).
//!
//! Submodules:
//! - [`lrucache`] — `resty.lrucache` (per-instance LRU; lua-resty-lrucache parity),
//! - [`shared_dict`] — `ngx.shared.<name>` worker-global dictionaries (ADR **Q15**),
//! - [`random`] — `resty.random` (CSPRNG bytes/token via `getrandom`),
//! - [`string`] — `resty.string` (base64/hex) + `resty.sha256` digest object.
//!
//! All storage is in-VM, single-threaded (`!Send`), mirroring openresty's
//! per-worker model (ADR **Q12**). `ngx.shared.DICT` lives in VM `app_data` so it
//! persists across requests on the same worker — the T5.3 acceptance gate.

use mlua::{Lua, Table};

mod http;
mod lock;
mod lrucache;
mod random;
mod shared_dict;
mod string;

/// Coerce a Lua key (string/number/boolean) to owned bytes for use as a map
/// key. Mirrors how openresty indexes its caches (numbers stringify).
pub(super) fn key_to_bytes(v: mlua::Value) -> mlua::Result<Vec<u8>> {
    use mlua::Value;
    Ok(match v {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Integer(i) => i.to_string().into_bytes(),
        Value::Number(n) => n.to_string().into_bytes(),
        Value::Boolean(b) => b.to_string().into_bytes(),
        Value::Nil => {
            return Err(mlua::Error::RuntimeError("nil key is not allowed".into()))
        }
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "key must be string/number/boolean (got {other:?})"
            )))
        }
    })
}

/// Register the `resty` global table and the `ngx.shared.DICT` zone on a worker
/// VM. Called once after [`crate::ngx::register`]; the `ngx` global must already
/// exist (we install `ngx.shared` onto it).
pub fn register(lua: &Lua) -> mlua::Result<()> {
    shared_dict::install_registry(lua);

    let resty = lua.create_table()?;
    resty.raw_set("lrucache", lrucache::build(lua)?)?;
    resty.raw_set("string", string::build_string(lua)?)?;
    resty.raw_set("sha256", string::build_sha256(lua)?)?;
    resty.raw_set("random", random::build(lua)?)?;
    resty.raw_set("http", http::build(lua)?)?;
    resty.raw_set("lock", lock::build(lua)?)?;
    lua.globals().set("resty", resty)?;

    // Install `ngx.shared` as a real field on the `ngx` global. Reads of
    // `ngx.shared` hit the raw table (set here) before `ngx`'s own metatable,
    // so no change to `ngx.__index` is needed. The shared proxy's OWN `__index`
    // auto-creates a named dict on first access (ADR Q15).
    let ngx = lua.globals().get::<Table>("ngx")?;
    ngx.raw_set("shared", shared_dict::build_shared_proxy(lua)?)?;
    Ok(())
}
