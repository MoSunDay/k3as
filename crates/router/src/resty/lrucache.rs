//! `resty.lrucache` — a per-instance, capacity-bounded LRU cache, parity with
//! lua-resty-lrucache (Scope A subset: `get`/`set`/`delete`/`flush_all`/`count`/
//! `get_keys`; TTL/exptime deferred).
//!
//! Stored values are arbitrary Lua types (tables included) — they survive across
//! calls because they are parked in the Lua registry via [`mlua::RegistryKey`]
//! and re-fetched by handle. A `resty.lrucache.new(size)` instance is a Lua
//! value; when held by a module-level local (or stashed in worker-global state)
//! it persists across requests on the same worker VM.
//!
//! The LRU is hand-written (no `lru` dependency, ADR Q4 minimalism): an access
//! `order` vector keyed alongside a `HashMap` of registry handles. `remove(0)`
//! eviction is O(n) but cache sizes are small in the Ingress use case.

use std::cell::RefCell;
use std::collections::HashMap;

use mlua::{Lua, RegistryKey, Table, UserData, UserDataMethods, Value};

use super::key_to_bytes;

/// Default entry count when `new(0|nil)`.
const DEFAULT_CAPACITY: usize = 1024;

/// The Lua-visible cache userdata.
pub struct LruCache {
    inner: RefCell<Inner>,
}

struct Inner {
    capacity: usize,
    /// Access order: front = least-recently-used, back = most-recently-used.
    order: Vec<Vec<u8>>,
    /// Key bytes -> registry handle to the stored value.
    entries: HashMap<Vec<u8>, RegistryKey>,
}

impl LruCache {
    fn new(capacity: usize) -> Self {
        Self {
            inner: RefCell::new(Inner {
                capacity: if capacity == 0 {
                    DEFAULT_CAPACITY
                } else {
                    capacity
                },
                order: Vec::new(),
                entries: HashMap::new(),
            }),
        }
    }
}

/// Build the `resty.lrucache` table (`{ new = fn }`).
pub(super) fn build(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set(
        "new",
        lua.create_function(|lua, size: Option<usize>| {
            lua.create_userdata(LruCache::new(size.unwrap_or(0)))
        })?,
    )?;
    Ok(t)
}

impl UserData for LruCache {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", get);
        methods.add_method("set", set);
        methods.add_method("delete", delete);
        methods.add_method("flush_all", flush_all);
        methods.add_method("count", count);
        methods.add_method("get_keys", get_keys);
    }
}

/// `cache:get(key)` -> stored value (any type) or nil. Touches the key (MRU).
///
/// Reads the registry value through a `&RegistryKey` borrow while holding the
/// cache RefCell borrow (the registry lookup does not touch our RefCell), then
/// reorders — `RegistryKey` is not `Clone`, so we cannot detach it.
fn get(lua: &Lua, this: &LruCache, key: Value) -> mlua::Result<Value> {
    let k = key_to_bytes(key)?;
    let mut g = this.inner.borrow_mut();
    match g.entries.get(&k) {
        None => Ok(Value::Nil),
        Some(rk) => {
            let val = lua.registry_value::<Value>(rk)?;
            touch(&mut g, &k);
            Ok(val)
        }
    }
}

/// `cache:set(key, value)` -> true. Replaces if present; evicts LRU over cap.
fn set(lua: &Lua, this: &LruCache, (key, value): (Value, Value)) -> mlua::Result<bool> {
    let k = key_to_bytes(key)?;
    let mut g = this.inner.borrow_mut();
    if let Some(old) = g.entries.remove(&k) {
        let _ = lua.remove_registry_value(old);
        g.order.retain(|x| x != &k);
    }
    let rk = lua.create_registry_value(value)?;
    g.entries.insert(k.clone(), rk);
    g.order.push(k);
    evict(lua, &mut g)?;
    Ok(true)
}

/// `cache:delete(key)` -> true (no error if absent).
fn delete(lua: &Lua, this: &LruCache, key: Value) -> mlua::Result<bool> {
    let k = key_to_bytes(key)?;
    let mut g = this.inner.borrow_mut();
    if let Some(rk) = g.entries.remove(&k) {
        let _ = lua.remove_registry_value(rk);
        g.order.retain(|x| x != &k);
    }
    Ok(true)
}

/// `cache:flush_all()` -> true.
fn flush_all(lua: &Lua, this: &LruCache, (): ()) -> mlua::Result<bool> {
    let mut g = this.inner.borrow_mut();
    for (_, rk) in g.entries.drain() {
        let _ = lua.remove_registry_value(rk);
    }
    g.order.clear();
    Ok(true)
}

/// `cache:count()` -> number of live entries.
fn count(_lua: &Lua, this: &LruCache, (): ()) -> mlua::Result<i64> {
    Ok(this.inner.borrow().entries.len() as i64)
}

/// `cache:get_keys(max?)` -> array of keys (most-recent first), up to `max`.
fn get_keys(lua: &Lua, this: &LruCache, max: Option<usize>) -> mlua::Result<Table> {
    let g = this.inner.borrow();
    let max = max.unwrap_or(1024);
    let tbl = lua.create_table()?;
    for (i, k) in g.order.iter().rev().take(max).enumerate() {
        tbl.set(i + 1, lua.create_string(k)?)?;
    }
    Ok(tbl)
}

/// Mark `k` most-recently-used: move it to the back of `order`.
fn touch(inner: &mut Inner, k: &[u8]) {
    inner.order.retain(|x| x != k);
    inner.order.push(k.to_vec());
}

/// Evict least-recently-used entries until within capacity, freeing registry
/// slots.
fn evict(lua: &Lua, inner: &mut Inner) -> mlua::Result<()> {
    while inner.order.len() > inner.capacity {
        let victim = inner.order.remove(0);
        if let Some(rk) = inner.entries.remove(&victim) {
            let _ = lua.remove_registry_value(rk);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_capacity_when_zero() {
        assert_eq!(LruCache::new(0).inner.borrow().capacity, DEFAULT_CAPACITY);
    }
    #[test]
    fn explicit_capacity() {
        assert_eq!(LruCache::new(7).inner.borrow().capacity, 7);
    }
}
