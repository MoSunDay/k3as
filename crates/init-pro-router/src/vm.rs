//! VM driver: a worker-wide Lua VM carrying per-coroutine Lua threads, driven
//! on a single-thread async runtime (the openresty worker model — ADR Q12).

use mlua::Lua;

use crate::ngx;

/// Build a fresh worker-wide LuaJIT VM with the `ngx.*` async primitives
/// registered. Mirrors openresty's per-worker `init_worker_by_lua` VM.
///
/// The returned `Lua` owns a single LuaJIT state. Per-request work runs as
/// separate Lua coroutines driven via `Function::call_async` (see
/// `tests/concurrency.rs`); they share the VM but interleave cooperatively at
/// `await` points. Drive the coroutines on a `tokio::task::LocalSet` so the
/// `!Send` VM never crosses a thread boundary.
pub fn worker_vm() -> mlua::Result<Lua> {
    let lua = Lua::new();
    ngx::register(&lua)?;
    Ok(lua)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Function;
    use tokio::task::LocalSet;

    /// A coroutine that calls the async `ngx.sleep` completes and returns its
    /// value — the minimum proof that the bridge drives a coroutine end to end.
    #[tokio::test]
    async fn coroutine_runs_async_sleep_and_returns() {
        LocalSet::new()
            .run_until(async {
                let lua = worker_vm().expect("worker vm");
                let f = lua
                    .load("return function() ngx.sleep(1) return 42 end")
                    .eval::<Function>()
                    .expect("lua function");
                let n: i64 = f.call_async::<i64>(()).await.expect("call_async");
                assert_eq!(n, 42);
            })
            .await;
    }
}
