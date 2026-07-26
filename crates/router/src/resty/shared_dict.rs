//! `ngx.shared.DICT` — worker-global, cross-request-persistent dictionaries
//! (TODO **T5.3**, ADR **Q15**).
//!
//! openresty pre-declares zones via `lua_shared_dict name size;`. We have no
//! such config yet (the Ingress config = T5.4), so a named dict is
//! **auto-created** on first access (`local d = ngx.shared.dogs`) with a default
//! entry capacity (Q15). The zone store lives in the VM's `app_data`, so
//! successive requests on the same worker observe the same entries — the T5.3
//! acceptance gate.
//!
//! Scope A subset (no TTL/exptime, no `safe_*`/`peek`/stale variants):
//! `get`/`set`/`add`/`replace`/`incr`/`delete`/`flush_all`/`get_keys`/`get_all`.
//! Values are scalars (string/number/boolean) — openresty's own restriction for
//! shared dicts. `incr` is single-threaded => naturally atomic (no interleaving).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Lua, LuaString, Table, UserData, UserDataMethods, Value};

use super::key_to_bytes;

/// Default entry count for an auto-created dict (Q15: no byte-size config).
const DEFAULT_DICT_CAPACITY: usize = 4096;

/// A scalar shared-dict value (openresty forbids tables in shared dicts).
#[derive(Clone)]
enum SharedValue {
    Bytes(Vec<u8>),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl SharedValue {
    fn from_lua(v: Value) -> mlua::Result<Self> {
        Ok(match v {
            Value::String(s) => SharedValue::Bytes(s.as_bytes().to_vec()),
            Value::Integer(i) => SharedValue::Int(i),
            Value::Number(n) => SharedValue::Float(n),
            Value::Boolean(b) => SharedValue::Bool(b),
            Value::Nil => {
                return Err(mlua::Error::RuntimeError(
                    "cannot store nil in ngx.shared.DICT".into(),
                ))
            }
            other => {
                return Err(mlua::Error::RuntimeError(format!(
                    "shared dict values must be string/number/boolean (got {other:?})"
                )))
            }
        })
    }

    fn to_value(&self, lua: &Lua) -> mlua::Result<Value> {
        Ok(match self {
            SharedValue::Bytes(b) => Value::String(lua.create_string(b)?),
            SharedValue::Int(i) => Value::Integer(*i),
            SharedValue::Float(n) => Value::Number(*n),
            SharedValue::Bool(b) => Value::Boolean(*b),
        })
    }

    /// Promote to f64 for `incr`. Non-numbers are rejected by the caller.
    fn as_f64(&self) -> Option<f64> {
        match self {
            SharedValue::Int(i) => Some(*i as f64),
            SharedValue::Float(n) => Some(*n),
            _ => None,
        }
    }
}

/// Backing store for one named dictionary.
pub(super) struct SharedDictStore {
    capacity: usize,
    entries: HashMap<Vec<u8>, SharedValue>,
}

impl SharedDictStore {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
        }
    }
}

/// Worker-global registry of named dicts: `name -> store`. Lives in `app_data`.
pub struct SharedDictRegistry(RefCell<HashMap<String, Rc<RefCell<SharedDictStore>>>>);

impl SharedDictRegistry {
    /// A fresh empty registry (one per worker VM).
    pub fn new() -> Self {
        Self(RefCell::new(HashMap::new()))
    }

    /// Look up (or lazily create, Q15) the store for `name`.
    fn get_or_create(&self, name: &str) -> Rc<RefCell<SharedDictStore>> {
        self.0
            .borrow_mut()
            .entry(name.to_owned())
            .or_insert_with(|| {
                Rc::new(RefCell::new(SharedDictStore::new(DEFAULT_DICT_CAPACITY)))
            })
            .clone()
    }
}

impl Default for SharedDictRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Install a fresh registry in VM app-data (idempotent).
pub(super) fn install_registry(lua: &Lua) {
    if lua.app_data_ref::<SharedDictRegistry>().is_none() {
        lua.set_app_data(SharedDictRegistry::new());
    }
}

/// Build the `ngx.shared` proxy: a table whose `__index` lazily creates a named
/// dict and returns a [`SharedDictHandle`] userdata bound to its store.
pub(super) fn build_shared_proxy(lua: &Lua) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.set(
        "__index",
        lua.create_function(|lua, (_self, name): (Table, LuaString)| {
            let name = name.to_str()?.to_owned();
            let store = lua
                .app_data_ref::<SharedDictRegistry>()
                .expect("SharedDictRegistry installed")
                .get_or_create(&name);
            lua.create_userdata(SharedDictHandle { name, store })
        })?,
    )?;
    proxy.set_metatable(Some(mt))?;
    Ok(proxy)
}

/// A Lua handle to one named dict. Cheap to hold; all handles for a name share
/// the same `Rc<RefCell<..>>` store (cross-request persistence).
pub struct SharedDictHandle {
    #[allow(dead_code)]
    name: String,
    store: Rc<RefCell<SharedDictStore>>,
}

impl UserData for SharedDictHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", get);
        methods.add_method("set", set);
        methods.add_method("add", add);
        methods.add_method("replace", replace);
        methods.add_method("incr", incr);
        methods.add_method("delete", delete);
        methods.add_method("flush_all", flush_all);
        methods.add_method("get_keys", get_keys);
        methods.add_method("get_all", get_all);
    }
}

/// `d:get(key)` -> value, or nil.
fn get(lua: &Lua, this: &SharedDictHandle, key: Value) -> mlua::Result<Value> {
    let k = key_to_bytes(key)?;
    match this.store.borrow().entries.get(&k).cloned() {
        Some(v) => v.to_value(lua),
        None => Ok(Value::Nil),
    }
}

/// `d:set(key, value)` -> true | (false, "no memory").
fn set(
    _lua: &Lua,
    this: &SharedDictHandle,
    (key, value): (Value, Value),
) -> mlua::Result<(bool, Option<String>)> {
    let k = key_to_bytes(key)?;
    let v = SharedValue::from_lua(value)?;
    let mut g = this.store.borrow_mut();
    let existed = g.entries.contains_key(&k);
    if !existed && g.entries.len() >= g.capacity {
        return Ok((false, Some("no memory".to_owned())));
    }
    g.entries.insert(k, v);
    Ok((true, None))
}

/// `d:add(key, value)` -> true | (false, "exists").
fn add(
    _lua: &Lua,
    this: &SharedDictHandle,
    (key, value): (Value, Value),
) -> mlua::Result<(bool, Option<String>)> {
    let k = key_to_bytes(key)?;
    let v = SharedValue::from_lua(value)?;
    let mut g = this.store.borrow_mut();
    if g.entries.contains_key(&k) {
        return Ok((false, Some("exists".to_owned())));
    }
    if g.entries.len() >= g.capacity {
        return Ok((false, Some("no memory".to_owned())));
    }
    g.entries.insert(k, v);
    Ok((true, None))
}

/// `d:replace(key, value)` -> true | (false, "not found").
fn replace(
    _lua: &Lua,
    this: &SharedDictHandle,
    (key, value): (Value, Value),
) -> mlua::Result<(bool, Option<String>)> {
    let k = key_to_bytes(key)?;
    let v = SharedValue::from_lua(value)?;
    let mut g = this.store.borrow_mut();
    if !g.entries.contains_key(&k) {
        return Ok((false, Some("not found".to_owned())));
    }
    g.entries.insert(k, v);
    Ok((true, None))
}

/// `d:incr(key, by, init?)` -> (newval | nil, err | nil). Single-threaded => atomic.
fn incr(
    _lua: &Lua,
    this: &SharedDictHandle,
    (key, by, init): (Value, f64, Option<f64>),
) -> mlua::Result<(Option<f64>, Option<String>)> {
    let k = key_to_bytes(key)?;
    let mut g = this.store.borrow_mut();
    let cur = g.entries.get(&k).cloned();
    let new = match (cur, init) {
        (Some(v), _) => match v.as_f64() {
            Some(n) => n + by,
            None => return Ok((None, Some("not a number".to_owned()))),
        },
        (None, Some(init)) => init + by,
        (None, None) => return Ok((None, Some("not found".to_owned()))),
    };
    // Keep integer storage when the result is integral.
    let stored = if new.fract() == 0.0 {
        SharedValue::Int(new as i64)
    } else {
        SharedValue::Float(new)
    };
    g.entries.insert(k, stored);
    Ok((Some(new), None))
}

/// `d:delete(key)` -> true (no error if absent).
fn delete(_lua: &Lua, this: &SharedDictHandle, key: Value) -> mlua::Result<bool> {
    let k = key_to_bytes(key)?;
    this.store.borrow_mut().entries.remove(&k);
    Ok(true)
}

/// `d:flush_all()` -> true.
fn flush_all(_lua: &Lua, this: &SharedDictHandle, (): ()) -> mlua::Result<bool> {
    this.store.borrow_mut().entries.clear();
    Ok(true)
}

/// `d:get_keys(max?)` -> array of key strings. `max` of 0/nil means all.
fn get_keys(lua: &Lua, this: &SharedDictHandle, max: Option<i64>) -> mlua::Result<Table> {
    let g = this.store.borrow();
    let limit = match max.unwrap_or(0) {
        0 => usize::MAX,
        n if n > 0 => n as usize,
        _ => 0,
    };
    let tbl = lua.create_table()?;
    for (i, k) in g.entries.keys().take(limit).enumerate() {
        tbl.set(i + 1, lua.create_string(k)?)?;
    }
    Ok(tbl)
}

/// `d:get_all()` -> `{ key = value, ... }` snapshot of all live entries.
fn get_all(lua: &Lua, this: &SharedDictHandle, (): ()) -> mlua::Result<Table> {
    let g = this.store.borrow();
    let tbl = lua.create_table()?;
    for (k, v) in g.entries.iter() {
        tbl.set(lua.create_string(k)?, v.to_value(lua)?)?;
    }
    Ok(tbl)
}
