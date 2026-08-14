//! Shared-index informer (T3.1a): the LIST -> WATCH reflector loop.
//!
//! Mirrors upstream client-go `sharedIndexInformer`: a full LIST seeds an
//! [`ObjectStore`] cache and fires synthetic initial events (so handlers
//! enqueue every existing object), then a replaying WATCH streams deltas.
//! When the watch closes (`recv() == None`: lag or backend shutdown) the
//! informer re-LISTs from scratch -- the upstream resync path.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::Value;
use storage::{KeyPrefix, StoredEntry, WatchEvent};

use crate::client::{namespace_of_path, set_namespace, Client};
use crate::object::{object_key, resource_version};
use crate::stop::Stop;

/// Consumer of informer events (typically a workqueue enqueue closure).
pub type EventHandler = Arc<dyn Fn(&WatchEvent) + Send + Sync>;

/// Thread-safe local cache of one resource's objects, keyed `ns/name`.
pub struct ObjectStore {
    items: RwLock<HashMap<String, Arc<Value>>>,
}

impl Default for ObjectStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectStore {
    pub fn new() -> Self {
        Self {
            items: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, namespace: &str, name: &str) -> Option<Arc<Value>> {
        self.items
            .read()
            .unwrap()
            .get(&format!("{namespace}/{name}"))
            .cloned()
    }

    /// All objects, optionally filtered to one namespace (key-sorted).
    pub fn list(&self, namespace: Option<&str>) -> Vec<Arc<Value>> {
        let prefix = namespace.map(|ns| format!("{ns}/"));
        let mut pairs: Vec<(String, Arc<Value>)> = self
            .items
            .read()
            .unwrap()
            .iter()
            .filter(|(k, _)| match &prefix {
                Some(p) => k.starts_with(p.as_str()),
                None => true,
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs.into_iter().map(|(_, v)| v).collect()
    }

    pub fn len(&self) -> usize {
        self.items.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Swap the whole map (re-LIST); returns the keys REMOVED so the caller
    /// can synthesize Delete events for them.
    pub(crate) fn replace_all(&self, objs: Vec<(String, Arc<Value>)>) -> Vec<String> {
        let new: HashMap<String, Arc<Value>> = objs.into_iter().collect();
        let mut old = self.items.write().unwrap();
        let removed: Vec<String> = old
            .keys()
            .filter(|k| !new.contains_key(*k))
            .cloned()
            .collect();
        *old = new;
        removed
    }

    /// Apply one watch event to the cache (idempotent). Put events are
    /// re-projected with `metadata.resourceVersion = mod_revision` so cache
    /// readers can CAS against the same revision the writer observed (raw
    /// stored payloads do not carry it).
    pub fn apply_event(&self, ev: &WatchEvent) {
        match ev {
            WatchEvent::Put(e) => {
                let mut value = e.value.clone();
                if let Some(meta) = value.get_mut("metadata").and_then(|m| m.as_object_mut()) {
                    meta.insert(
                        "resourceVersion".to_string(),
                        Value::String(e.mod_revision.to_string()),
                    );
                }
                self.items
                    .write()
                    .unwrap()
                    .insert(object_key(&value), Arc::new(value));
            }
            WatchEvent::Delete { key, prev, .. } => {
                let k = prev
                    .as_ref()
                    .map(|p| object_key(&p.value))
                    .unwrap_or_else(|| key_from_path(key));
                self.items.write().unwrap().remove(&k);
            }
        }
    }
}

/// Derive `ns/name` from a storage path as a last resort (no prev object on
/// a synthetic delete). Caveat: for cluster-scoped paths this returns
/// `resource/name`; v1 controllers only inform on namespaced resources.
pub fn key_from_path(path: &str) -> String {
    let segs: Vec<&str> = path
        .strip_prefix("/registry/")
        .unwrap_or(path)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    match segs.len() {
        0 => String::new(),
        1 => segs[0].to_string(),
        n => format!("{}/{}", segs[n - 2], segs[n - 1]),
    }
}

/// The `ns/name` key of the object an event refers to.
pub fn event_object_key(ev: &WatchEvent) -> Option<String> {
    match ev {
        WatchEvent::Put(e) => Some(object_key(&e.value)),
        WatchEvent::Delete { prev, key, .. } => Some(match prev {
            Some(p) => object_key(&p.value),
            None => key_from_path(key),
        }),
    }
}

/// Defensive payload normalization: a writer that omits
/// `metadata.namespace` (an apiserver replace path did before the URI
/// defaulting landed) would otherwise upsert the cache under a bare `name`
/// key while the real `ns/name` entry goes stale forever -- the reconcile
/// then no-ops against the zombie object. Keying from the authoritative
/// storage path (group-aware) closes the class for every writer.
fn normalize_event_namespace(ev: WatchEvent, prefix: &KeyPrefix) -> WatchEvent {
    match ev {
        WatchEvent::Put(e) => {
            let has_ns = e
                .value
                .pointer("/metadata/namespace")
                .and_then(Value::as_str)
                .is_some_and(|ns| !ns.is_empty());
            if has_ns {
                return WatchEvent::Put(e);
            }
            let Some(ns) = namespace_of_path(&e.key, &prefix.group) else {
                return WatchEvent::Put(e); // cluster-scoped path shape
            };
            let mut value = e.value.clone();
            set_namespace(&mut value, &ns);
            WatchEvent::Put(Arc::new(StoredEntry {
                key: e.key.clone(),
                value,
                create_revision: e.create_revision,
                mod_revision: e.mod_revision,
                version: e.version,
            }))
        }
        other => other,
    }
}

/// The object an event refers to (the new value, or the deleted one).
pub fn event_object(ev: &WatchEvent) -> Option<Value> {
    match ev {
        WatchEvent::Put(e) => Some(e.value.clone()),
        WatchEvent::Delete { prev, .. } => prev.as_ref().map(|p| p.value.clone()),
    }
}

/// One reflector over a collection prefix.
pub struct Informer {
    prefix: KeyPrefix,
}

impl Informer {
    pub fn new(prefix: KeyPrefix) -> Self {
        Self { prefix }
    }

    /// Run the LIST -> WATCH loop until `stop` fires.
    pub async fn run(
        &self,
        client: Arc<dyn Client>,
        store: Arc<ObjectStore>,
        handler: EventHandler,
        stop: Stop,
    ) {
        loop {
            // Phase 1: full LIST seeds the cache + initial enqueue events.
            let listed = tokio::select! {
                _ = stop.cancelled() => return,
                res = client.list(&self.prefix) => match res {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(prefix = %self.prefix.as_path(), error = %e,
                            "informer list failed; retrying");
                        pause(&stop).await;
                        continue;
                    }
                },
            };
            let mut max_rev = 0u64;
            let mut items = Vec::with_capacity(listed.len());
            for v in listed {
                max_rev = max_rev.max(resource_version(&v).unwrap_or(0));
                items.push((object_key(&v), Arc::new(v)));
            }
            let removed = store.replace_all(items.clone());
            for (k, v) in &items {
                let ev = WatchEvent::Put(Arc::new(StoredEntry {
                    key: self.synthetic_path(k),
                    value: v.as_ref().clone(),
                    create_revision: 0, // unknown through the Value projection
                    mod_revision: resource_version(v).unwrap_or(0),
                    version: 0,
                }));
                handler(&ev);
            }
            for k in removed {
                let ev = WatchEvent::Delete {
                    key: self.synthetic_path(&k),
                    mod_revision: max_rev.max(1),
                    prev: None,
                };
                handler(&ev);
            }

            // Phase 2: replaying watch. `max_rev.max(1)` is INCLUSIVE so the
            // LIST->watch seam is bridged; `Some(0)` would be live-only and
            // could miss writes between the LIST and the subscription.
            let start = max_rev.max(1);
            let mut w = tokio::select! {
                _ = stop.cancelled() => return,
                res = client.watch(&self.prefix, Some(start)) => match res {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::warn!(prefix = %self.prefix.as_path(), error = %e,
                            "informer watch failed; re-listing");
                        pause(&stop).await;
                        continue;
                    }
                },
            };
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    ev = w.recv() => match ev {
                        Some(ev) => {
                            let ev = normalize_event_namespace(ev, &self.prefix);
                            let rev = ev.revision();
                            store.apply_event(&ev);
                            handler(&ev);
                            max_rev = max_rev.max(rev);
                        }
                        None => {
                            tracing::debug!(prefix = %self.prefix.as_path(),
                                "watch closed (lag or backend shutdown); re-listing");
                            break;
                        }
                    },
                }
            }
            pause(&stop).await; // pace the re-list (avoids a hot loop on fast closes)
        }
    }

    /// Storage path for a `ns/name` cache key under this prefix.
    fn synthetic_path(&self, obj_key: &str) -> String {
        match &self.prefix.namespace {
            // Namespaced prefix already ends at the namespace segment.
            Some(_) => format!(
                "{}/{}",
                self.prefix.as_path(),
                obj_key.rsplit('/').next().unwrap_or(obj_key)
            ),
            None => format!("{}/{}", self.prefix.as_path(), obj_key),
        }
    }
}

async fn pause(stop: &Stop) {
    tokio::select! {
        _ = stop.cancelled() => {}
        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
    }
}
