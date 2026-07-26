//! `ngx.*` async primitives for the Router VM.
//!
//! T5.1 scope: `ngx.sleep` only — the canonical yield primitive exercised by
//! the kill-criterion concurrency test. cosocket (`ngx.socket`), phase hooks,
//! and `ngx.location.capture` are T5.2 / T5.3.

use std::time::Duration;

use mlua::Lua;

/// Register the `ngx` table on a worker VM.
///
/// `ngx.sleep(ms)` maps to `tokio::time::sleep`, registered as an **async**
/// Rust function. When Lua code inside a coroutine calls it, `mlua` parks the
/// coroutine, polls the Rust future, and resumes the coroutine once it
/// completes — the coroutine<->async bridge at the heart of T5.1.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    let sleep = lua.create_async_function(|_lua, ms: u64| async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(())
    })?;

    let ngx = lua.create_table()?;
    ngx.set("sleep", sleep)?;
    lua.globals().set("ngx", ngx)?;
    Ok(())
}
