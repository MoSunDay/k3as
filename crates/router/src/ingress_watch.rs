//! Ingress reflector (workstream A, decision **Q32**): a LIST -> WATCH ->
//! re-LIST-on-close loop folding every `networking.k8s.io/v1` `Ingress`
//! into the resolver's third watch channel (mirrors
//! [`crate::endpoints_watch`], S4/Q28 informer pattern).
//!
//! The gateway consumes the folded set via
//! [`crate::endpoints::EndpointsResolver::ingress_table`] and recompiles its
//! openrusty config on every publish ([`crate::gateway`]).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::networking::Ingress;
use infra::Shutdown;
use storage::{KeyPrefix, StorageBackend};
use tokio::sync::watch;

/// Backoff before re-LISTing once a watch stream closes (parity with the
/// service-plane reflectors).
const RECONNECT_DELAY: Duration = Duration::from_millis(100);

/// The folded Ingress set, keyed `ns/name` (matches the Services/Endpoints
/// fold keys, so gateway logs line up).
pub type IngressMap = BTreeMap<String, Ingress>;

/// `/registry/networking.k8s.io/ingresses` — the apiserver's canonical key
/// scheme for the group.
pub fn ingresses_prefix() -> KeyPrefix {
    KeyPrefix::new("networking.k8s.io", "ingresses", None)
}

/// Parse a stored Ingress document; `None` = malformed (skip it).
pub(crate) fn parse_ingress(value: &serde_json::Value) -> Option<Ingress> {
    let name = value.get("metadata")?.get("name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    serde_json::from_value(value.clone()).ok()
}

/// `ns/name` from a stored Ingress document.
pub(crate) fn ingress_key(value: &serde_json::Value) -> Option<String> {
    let meta = value.get("metadata")?;
    let name = meta.get("name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    let ns = meta
        .get("namespace")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("default");
    Some(format!("{ns}/{name}"))
}

/// Replace the map wholesale (the re-LIST step).
pub(crate) fn replace_all(state: &mut IngressMap, entries: &[storage::StoredEntry]) {
    state.clear();
    for entry in entries {
        insert_or_drop(state, &entry.value, &entry.key);
    }
}

/// Insert (or drop, when malformed) one document keyed `ns/name`.
fn insert_or_drop(state: &mut IngressMap, value: &serde_json::Value, path: &str) {
    let Some(key) = ingress_key(value).or_else(|| key_from_path(path)) else {
        return;
    };
    match parse_ingress(value) {
        Some(ing) => {
            state.insert(key, ing);
        }
        None => {
            state.remove(&key);
            tracing::warn!(target: "init-pro", key = %key, "malformed Ingress skipped");
        }
    }
}

/// `ns/name` from a `/registry/<group>/<resource>/[ns]/<name>` storage path
/// (delete events without a usable `prev` payload): last two segments,
/// informer-style — same rule as the service plane.
fn key_from_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/registry/").unwrap_or(path);
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match segs.len() {
        0 | 1 => None,
        n => Some(format!("{}/{}", segs[n - 2], segs[n - 1])),
    }
}

/// Fold one watch event into the map (idempotent; a malformed Put drops the
/// key rather than poisoning it).
pub(crate) fn fold_event(state: &mut IngressMap, ev: &storage::WatchEvent) {
    match ev {
        storage::WatchEvent::Put(e) => insert_or_drop(state, &e.value, &e.key),
        storage::WatchEvent::Delete { key, prev, .. } => {
            let k = prev
                .as_ref()
                .and_then(|p| ingress_key(&p.value))
                .or_else(|| key_from_path(key));
            if let Some(k) = k {
                state.remove(&k);
            }
        }
    }
}

/// The Ingress LIST -> WATCH loop (gap-free: snapshot the revision first,
/// then LIST and watch from `rev + 1`; folds are idempotent so replays of
/// already-seen entries are harmless).
pub(crate) async fn watch_ingresses(
    store: Arc<dyn StorageBackend>,
    state: Arc<Mutex<IngressMap>>,
    tx: watch::Sender<Arc<IngressMap>>,
    shutdown: Shutdown,
) {
    let prefix = ingresses_prefix();
    loop {
        let rev = tokio::select! {
            _ = shutdown.cancelled() => return,
            res = store.current_revision() => match res {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(target: "init-pro", error = %e, "ingress reflector revision read failed; retrying");
                    if backoff(&shutdown).await { return; }
                    continue;
                }
            },
        };
        let entries = tokio::select! {
            _ = shutdown.cancelled() => return,
            res = store.list(&prefix) => match res {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(target: "init-pro", error = %e, "ingress reflector LIST failed; retrying");
                    if backoff(&shutdown).await { return; }
                    continue;
                }
            },
        };
        {
            let mut st = state.lock().expect("ingress state poisoned");
            replace_all(&mut st, &entries);
            let _ = tx.send_replace(Arc::new(st.clone()));
        }
        let watch = tokio::select! {
            _ = shutdown.cancelled() => return,
            res = store.watch(&prefix, Some(rev + 1)) => match res {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(target: "init-pro", error = %e, "ingress reflector WATCH failed; re-LISTing");
                    if backoff(&shutdown).await { return; }
                    continue;
                }
            },
        };
        tracing::info!(target: "init-pro", revision = rev, "ingress reflector watching");
        let mut watch = watch;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                ev = watch.recv() => match ev {
                    Some(ev) => {
                        let mut st = state.lock().expect("ingress state poisoned");
                        fold_event(&mut st, &ev);
                        let _ = tx.send_replace(Arc::new(st.clone()));
                    }
                    None => break, // stream closed -> re-LIST
                },
            }
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
        }
    }
}

/// Sleep for [`RECONNECT_DELAY`] unless shutdown fires. `true` = shut down.
async fn backoff(shutdown: &Shutdown) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => true,
        _ = tokio::time::sleep(RECONNECT_DELAY) => false,
    }
}

#[cfg(test)]
mod storage_replay {
    /// Full storage path (Q32): EmbeddedStorage LIST -> `replace_all` fold
    /// -> compile -> expressible -> lookup, with the exact document shapes
    /// the integration tests write. Guards the reflector's key-prefix and
    /// serde contracts end to end.
    use super::*;
    use serde_json::json;

    fn ingress_doc() -> serde_json::Value {
        json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
            "metadata": {"name": "edge", "namespace": "default"},
            "spec": {"rules": [{
                "host": "api.example.com",
                "http": {"paths": [
                    {"path": "/v1", "pathType": "Prefix",
                     "backend": {"service": {"name": "api", "port": {"number": 80}}}},
                    {"path": "/exact", "pathType": "Exact",
                     "backend": {"service": {"name": "api", "port": {"number": 80}}}}
                ]}
            }], "defaultBackend": {"service": {"name": "edge", "port": {"number": 80}}}}
        })
    }

    #[tokio::test]
    async fn storage_list_folds_and_compiles() {
        let store = storage::EmbeddedStorage::new();
        store
            .create(
                &storage::Key::new("networking.k8s.io", "ingresses", "default", "edge"),
                ingress_doc(),
            )
            .await
            .expect("create");
        let prefix = ingresses_prefix();
        let entries = store.list(&prefix).await.expect("list");
        let mut map = IngressMap::default();
        replace_all(&mut map, &entries);
        let ings: Vec<api::networking::Ingress> = map.values().cloned().collect();
        let table = crate::ingress::compile_ingress(&ings);
        let table = crate::gateway_config::expressible(&table);
        assert_eq!(map.len(), 1, "fold must keep the ingress");
        let hit = table
            .lookup("api.example.com", "/v1/users")
            .expect("prefix rule matches under the host");
        assert_eq!(hit.upstream.service, "api");
        assert!(
            table.lookup("other.example.com", "/").is_none(),
            "host rules must not leak across hosts (default backend is the gate's fallback)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc as StdArc;

    fn put(key: &str, value: serde_json::Value) -> storage::WatchEvent {
        storage::WatchEvent::Put(StdArc::new(storage::StoredEntry {
            key: key.to_string(),
            value,
            create_revision: 1,
            mod_revision: 1,
            version: 1,
        }))
    }

    fn ingress_doc(ns: &str, name: &str) -> serde_json::Value {
        json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
            "metadata": {"name": name, "namespace": ns},
            "spec": {"rules": [{"host": format!("{name}.io")}]},
        })
    }

    #[test]
    fn keys_and_parse_round_trip() {
        let doc = ingress_doc("app", "edge");
        assert_eq!(ingress_key(&doc).as_deref(), Some("app/edge"));
        let ing = parse_ingress(&doc).expect("parses");
        assert_eq!(ing.metadata.name.as_deref(), Some("edge"));
        assert!(ingress_key(&json!({"metadata": {"name": ""}})).is_none());
        assert!(parse_ingress(&json!({"metadata": {}})).is_none());
    }

    #[test]
    fn replace_all_and_fold_events_track_the_map() {
        let mut state = IngressMap::default();
        replace_all(
            &mut state,
            &[
                storage::StoredEntry {
                    key: "/registry/networking.k8s.io/ingresses/app/edge".into(),
                    value: ingress_doc("app", "edge"),
                    create_revision: 1,
                    mod_revision: 1,
                    version: 1,
                },
                storage::StoredEntry {
                    key: "/registry/networking.k8s.io/ingresses/app/bad".into(),
                    value: json!({"kind": "Ingress"}),
                    create_revision: 1,
                    mod_revision: 1,
                    version: 1,
                },
            ],
        );
        assert_eq!(state.len(), 1, "malformed entries skipped");
        assert!(state.contains_key("app/edge"));

        fold_event(
            &mut state,
            &put(
                "/registry/networking.k8s.io/ingresses/app/edge2",
                ingress_doc("app", "edge2"),
            ),
        );
        assert_eq!(state.len(), 2);
        fold_event(
            &mut state,
            &storage::WatchEvent::Delete {
                key: "/registry/networking.k8s.io/ingresses/app/edge".into(),
                mod_revision: 2,
                prev: None,
            },
        );
        assert!(state.contains_key("app/edge2"));
        assert!(!state.contains_key("app/edge"));
    }
}
