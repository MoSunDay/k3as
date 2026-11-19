//! `TrafficSplit` reflector (TODO **T8.2**, decision **Q57**).
//!
//! LIST -> WATCH -> re-LIST-on-close loop folding every
//! `init-pro.io/v1` `TrafficSplit` into the resolver state's `splits` map
//! ([`crate::split::SplitView`]). The NodePort plane re-plans weighted
//! peers on each fold, so a weight edit is a hot reload of the openrusty
//! upstream (Q32 reload seam).
//!
//! Mirrors the service-plane reflectors: snapshot the revision, LIST,
//! watch from `rev + 1`, folds are idempotent so replayed entries are
//! harmless.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use infra::Shutdown;
use storage::{KeyPrefix, StorageBackend, StoredEntry, WatchEvent};
use tokio::sync::watch;

use crate::endpoints::{ns_name_from_path, ns_name_of, ResolverState};
use crate::split::SplitView;

/// Backoff before re-LISTing once a watch stream closes.
const RECONNECT_DELAY: Duration = Duration::from_millis(100);

/// `/registry/init-pro.io/trafficsplits` — the apiserver's canonical key
/// scheme for the init-pro.io group.
pub fn trafficsplits_prefix() -> KeyPrefix {
    KeyPrefix::new("init-pro.io", "trafficsplits", None)
}

/// Parse a stored TrafficSplit document; `None` = malformed (skip it).
pub(crate) fn parse_split(value: &serde_json::Value) -> Option<SplitView> {
    let ts: api::traffic::TrafficSplit = serde_json::from_value(value.clone()).ok()?;
    SplitView::from_api(&ts)
}

/// Replace the split map wholesale (the re-LIST step).
pub(crate) fn replace_all(state: &mut ResolverState, entries: &[StoredEntry]) {
    state.splits = entries
        .iter()
        .filter_map(|e| Some((ns_name_of(&e.value)?, parse_split(&e.value)?)))
        .collect();
}

/// Fold one watch event into the state (idempotent; a malformed Put drops
/// the key rather than poisoning it).
pub(crate) fn fold_event(state: &mut ResolverState, ev: &WatchEvent) {
    match ev {
        WatchEvent::Put(e) => {
            let Some(key) = ns_name_of(&e.value).or_else(|| ns_name_from_path(&e.key)) else {
                return;
            };
            match parse_split(&e.value) {
                Some(v) => {
                    state.splits.insert(key, v);
                }
                None => {
                    state.splits.remove(&key);
                    tracing::warn!(target: "init-pro", key = %key, "malformed TrafficSplit skipped");
                }
            }
        }
        WatchEvent::Delete { key, prev, .. } => {
            let k = prev
                .as_ref()
                .and_then(|p| ns_name_of(&p.value))
                .or_else(|| ns_name_from_path(key));
            if let Some(k) = k {
                state.splits.remove(&k);
            }
        }
    }
}

/// The TrafficSplit LIST -> WATCH loop; publishes a new snapshot per fold
/// through the shared resolver channel.
pub(crate) async fn watch_splits(
    store: Arc<dyn StorageBackend>,
    state: Arc<Mutex<ResolverState>>,
    tx: watch::Sender<Arc<ResolverState>>,
    shutdown: Shutdown,
) {
    let prefix = trafficsplits_prefix();
    loop {
        let rev = tokio::select! {
            _ = shutdown.cancelled() => return,
            res = store.current_revision() => match res {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(prefix = %prefix.as_path(), error = %e,
                        "trafficsplit revision read failed; retrying");
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
                    tracing::warn!(prefix = %prefix.as_path(), error = %e,
                        "trafficsplit LIST failed; retrying");
                    if backoff(&shutdown).await { return; }
                    continue;
                }
            },
        };
        {
            let mut st = state.lock().expect("trafficsplit state poisoned");
            replace_all(&mut st, &entries);
            let _ = tx.send_replace(Arc::new(st.clone()));
        }
        let watch = tokio::select! {
            _ = shutdown.cancelled() => return,
            res = store.watch(&prefix, Some(rev + 1)) => match res {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(prefix = %prefix.as_path(), error = %e,
                        "trafficsplit WATCH failed; re-LISTing");
                    if backoff(&shutdown).await { return; }
                    continue;
                }
            },
        };
        tracing::info!(prefix = %prefix.as_path(), revision = rev,
            "trafficsplit reflector watching");
        let mut watch = watch;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                ev = watch.recv() => match ev {
                    Some(ev) => {
                        let mut st = state.lock().expect("trafficsplit state poisoned");
                        fold_event(&mut st, &ev);
                        let _ = tx.send_replace(Arc::new(st.clone()));
                    }
                    None => break,
                },
            }
        }
        if backoff(&shutdown).await {
            return;
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
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_folds_camel_case_and_defaults() {
        let v = json!({
            "apiVersion": "init-pro.io/v1",
            "kind": "TrafficSplit",
            "metadata": {"name": "app", "namespace": "prod"},
            "spec": {"service": "app", "backends": [
                {"service": "app-stable", "weight": 90},
                {"service": "app-canary", "weight": 10}
            ]}
        });
        let split = parse_split(&v).expect("parses");
        assert_eq!(split.namespace, "prod");
        assert_eq!(split.backends.len(), 2);
        assert_eq!(ns_name_of(&v).as_deref(), Some("prod/app"));
    }

    #[test]
    fn malformed_split_is_rejected() {
        let v = json!({"kind": "TrafficSplit", "metadata": {"name": "x"}, "spec": {}});
        assert!(parse_split(&v).is_none());
    }

    #[test]
    fn delete_events_drop_by_prev_or_path() {
        let mut st = ResolverState::default();
        st.splits.insert(
            "default/app".into(),
            SplitView {
                namespace: "default".into(),
                name: "app".into(),
                service: "app".into(),
                backends: Vec::new(),
            },
        );
        let ev = WatchEvent::Delete {
            key: "/registry/init-pro.io/trafficsplits/default/app".into(),
            mod_revision: 1,
            prev: None,
        };
        fold_event(&mut st, &ev);
        assert!(st.splits.is_empty());
    }
}
