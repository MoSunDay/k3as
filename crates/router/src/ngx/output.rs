//! Response-output primitives: the async bridge (`sleep`) and the per-request
//! response writers (`say`/`print`/`exit`), plus the openresty time helpers
//! (`now`/`time`/`update_time`). All resolve the live request via the Q13
//! coroutine-local store.

use std::cell::Cell;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mlua::{Lua, Table};

use crate::context;
use crate::ngx::{coerce_u16, value_to_string, EXIT_SENTINEL};

/// A cached "current time" epoch (seconds) that `ngx.update_time` refreshes and
/// `ngx.now`/`ngx.time` read — mirrors openresty's per-worker time cache so the
/// cost is one syscall per phase, not one per call. Stored in VM app-data.
pub(super) struct TimeCache {
    epoch_secs: Cell<f64>,
}

impl TimeCache {
    fn refresh(&self) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        self.epoch_secs.set(secs);
    }
}

fn ensure_time_cache(lua: &Lua) {
    if lua.app_data_ref::<TimeCache>().is_none() {
        let c = TimeCache {
            epoch_secs: Cell::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
            ),
        };
        lua.set_app_data(c);
    }
}

/// Install `sleep`/`say`/`print`/`exit`/`now`/`time`/`update_time` on `ngx`.
pub(super) fn install(lua: &Lua, ngx: &Table) -> mlua::Result<()> {
    ensure_time_cache(lua);

    let sleep = lua.create_async_function(|_lua, ms: u64| async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(())
    })?;
    ngx.raw_set("sleep", sleep)?;

    ngx.raw_set("say", lua.create_function(say)?)?;
    ngx.raw_set("print", lua.create_function(print_body)?)?;
    ngx.raw_set("exit", lua.create_function(exit)?)?;

    ngx.raw_set("now", lua.create_function(now)?)?;
    ngx.raw_set("time", lua.create_function(time)?)?;
    ngx.raw_set("update_time", lua.create_function(update_time)?)?;
    Ok(())
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
/// `code` of 0 (openresty `ngx.OK`) just ends the phase without changing status.
fn exit(lua: &Lua, code: i32) -> mlua::Result<()> {
    let ctx = context::current(lua)?;
    {
        let mut g = ctx.borrow_mut();
        if code != 0 && g.status == 200 {
            let _ = coerce_u16(&mlua::Value::Integer(code as i64), "ngx.exit")?;
            g.status = code.clamp(100, 599) as u16;
        }
        g.exit_code = Some(code);
    }
    Err(mlua::Error::RuntimeError(EXIT_SENTINEL.to_owned()))
}

/// `ngx.now()` -> seconds since epoch, with sub-second precision (cached).
fn now(lua: &Lua, (): ()) -> mlua::Result<f64> {
    let cache = lua
        .app_data_ref::<TimeCache>()
        .expect("TimeCache installed");
    Ok(cache.epoch_secs.get())
}

/// `ngx.time()` -> integer seconds since epoch (cached).
fn time(lua: &Lua, (): ()) -> mlua::Result<i64> {
    let cache = lua
        .app_data_ref::<TimeCache>()
        .expect("TimeCache installed");
    Ok(cache.epoch_secs.get() as i64)
}

/// `ngx.update_time()` -> refresh the per-worker time cache from the clock.
fn update_time(lua: &Lua, (): ()) -> mlua::Result<()> {
    let cache = lua
        .app_data_ref::<TimeCache>()
        .expect("TimeCache installed");
    cache.refresh();
    Ok(())
}

/// Append the variadic Lua args to `out`, coerced to strings (openresty joins
/// `say`/`print` args with NO separator, unlike Lua's global `print`).
fn append_args(out: &mut Vec<u8>, args: mlua::MultiValue) -> mlua::Result<()> {
    for v in args {
        out.extend_from_slice(value_to_string(v)?.as_bytes());
    }
    Ok(())
}
