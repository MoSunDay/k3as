//! `resty.lock` — expiring-key mutual-exclusion locks (T5.3 Scope B).
//!
//! Parity with `lua-resty-lock`: a named lock object backed by a worker-global
//! lock zone. `lock(key)` acquires exclusively (yielding to other coroutines
//! until free or the timeout elapses); `unlock()` releases. Locks auto-expire
//! after `exptime` seconds so a crashed holder can't deadlock the worker.
//!
//! Because the VM is single-threaded (ADR **Q12**), "contention" only arises
//! when one coroutine yields while holding — exactly openresty's model. The
//! zone lives in `app_data`, so locks are visible across all coroutines on the
//! same worker (the T5.3 cross-coroutine gate).

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use mlua::{Lua, Table, UserData, UserDataMethods, UserDataRef};

/// Worker-global lock zone: `(dict, key)` -> expiry. Stored in `app_data`.
pub struct LockRegistry(RefCell<HashMap<(String, String), Instant>>);

impl LockRegistry {
    fn new() -> Self {
        Self(RefCell::new(HashMap::new()))
    }

    fn acquire(&self, dict: &str, key: &str, exptime: Duration) -> bool {
        let mut g = self.0.borrow_mut();
        let now = Instant::now();
        match g.get(&(dict.to_owned(), key.to_owned())) {
            Some(&exp) if exp > now => false,
            _ => {
                g.insert((dict.to_owned(), key.to_owned()), now + exptime);
                true
            }
        }
    }

    fn release(&self, dict: &str, key: &str) -> bool {
        self.0
            .borrow_mut()
            .remove(&(dict.to_owned(), key.to_owned()))
            .is_some()
    }
}

/// Build the `resty.lock` table: `{ new = fn }`.
pub fn build(lua: &Lua) -> mlua::Result<Table> {
    // Ensure the worker-global registry exists before any lock is created.
    if lua.app_data_ref::<LockRegistry>().is_none() {
        lua.set_app_data(LockRegistry::new());
    }
    let t = lua.create_table()?;
    t.raw_set("new", lua.create_function(new)?)?;
    Ok(t)
}

/// `resty.lock.new(dict, opts?)` -> lock userdata.
fn new(_lua: &Lua, (dict, opts): (String, LockOpts)) -> mlua::Result<Lock> {
    Ok(Lock {
        dict,
        timeout_ms: opts.timeout_ms.unwrap_or(5000),
        exptime_sec: opts.exptime_sec.unwrap_or(30),
        held: RefCell::new(None),
    })
}

/// A named lock object.
pub struct Lock {
    dict: String,
    timeout_ms: u64,
    exptime_sec: u64,
    held: RefCell<Option<String>>,
}

impl UserData for Lock {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method("lock", lock);
        methods.add_async_method("unlock", unlock);
    }
}

/// `lock:lock(key)` -> `elapsed_seconds` (number) or `nil, "timeout"`.
async fn lock(
    _lua: Lua,
    this: UserDataRef<Lock>,
    key: String,
) -> mlua::Result<(Option<f64>, Option<String>)> {
    let started = Instant::now();
    let deadline = started + Duration::from_millis(this.timeout_ms);
    let exptime = Duration::from_secs(this.exptime_sec.max(1));

    loop {
        let got = with_registry(&_lua, |r| r.acquire(&this.dict, &key, exptime))?;
        if got {
            *this.held.borrow_mut() = Some(key.clone());
            return Ok((Some(started.elapsed().as_secs_f64()), None));
        }
        if Instant::now() >= deadline {
            return Ok((None, Some("timeout".into())));
        }
        // Yield so a sibling coroutine holding the lock can run and release.
        tokio::task::yield_now().await;
    }
}

/// `lock:unlock()` -> `true` (always; double-unlock is a no-op, like openresty).
async fn unlock(_lua: Lua, this: UserDataRef<Lock>, (): ()) -> mlua::Result<bool> {
    if let Some(key) = this.held.borrow_mut().take() {
        let _ = with_registry(&_lua, |r| r.release(&this.dict, &key));
    }
    Ok(true)
}

fn with_registry<F, R>(lua: &Lua, f: F) -> mlua::Result<R>
where
    F: FnOnce(&LockRegistry) -> R,
{
    let r = lua
        .app_data_ref::<LockRegistry>()
        .ok_or_else(|| mlua::Error::RuntimeError("resty.lock: registry not installed".into()))?;
    Ok(f(&r))
}

struct LockOpts {
    timeout_ms: Option<u64>,
    exptime_sec: Option<u64>,
}

impl mlua::FromLua for LockOpts {
    fn from_lua(v: mlua::Value, _lua: &Lua) -> mlua::Result<Self> {
        match v {
            mlua::Value::Table(t) => Ok(LockOpts {
                timeout_ms: t.get("timeout").ok(),
                exptime_sec: t.get("exptime").ok(),
            }),
            _ => Ok(LockOpts {
                timeout_ms: None,
                exptime_sec: None,
            }),
        }
    }
}
