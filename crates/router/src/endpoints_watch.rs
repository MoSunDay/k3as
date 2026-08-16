//! LIST->WATCH reflectors for the service plane (Sprint 18 / **S4**, Q28 —
//! recorded S7): two storage watch loops fold the cluster's Services +
//! Endpoints into one watch channel shared by
//! [`crate::endpoints::EndpointsResolver`] (peer resolution) and
//! [`crate::nodeport`] (listener reconciliation). Mirrors the controllers'
//! informer LIST -> WATCH -> re-LIST-on-close pattern (T3.1a).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use infra::Shutdown;
use storage::{KeyPrefix, StorageBackend};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::endpoints::{fold_event, replace_all, EndpointsResolver, ResolverState, Resource};

/// Backoff before re-LISTing once a watch stream closes.
const RECONNECT_DELAY: Duration = Duration::from_millis(100);

/// `/registry/services` — the apiserver's canonical key scheme.
pub fn services_prefix() -> KeyPrefix {
    KeyPrefix::new("", "services", None)
}

/// `/registry/endpoints`.
pub fn endpoints_prefix() -> KeyPrefix {
    KeyPrefix::new("", "endpoints", None)
}

/// Spawn both reflector loops over `store`; returns the live resolver plus
/// the loop handles (they drain on `shutdown`).
pub fn supervise(
    store: Arc<dyn StorageBackend>,
    shutdown: Shutdown,
) -> (EndpointsResolver, Vec<JoinHandle<()>>) {
    let (tx, rx) = watch::channel(Arc::new(ResolverState::default()));
    let state = Arc::new(Mutex::new(ResolverState::default()));
    let services = tokio::spawn(watch_prefix(
        store.clone(),
        Resource::Services,
        services_prefix(),
        state.clone(),
        tx.clone(),
        shutdown.clone(),
    ));
    let endpoints = tokio::spawn(watch_prefix(
        store,
        Resource::Endpoints,
        endpoints_prefix(),
        state,
        tx,
        shutdown,
    ));
    (EndpointsResolver { rx }, vec![services, endpoints])
}

/// Fold `entries` (or one event) into the shared state and publish a new
/// snapshot. The mutex serializes the two loops; folds are idempotent.
fn publish(tx: &watch::Sender<Arc<ResolverState>>, state: &ResolverState) {
    let _ = tx.send_replace(Arc::new(state.clone()));
}

/// One resource's LIST -> WATCH loop; on `recv() == None` (stream closed:
/// compaction or backend restart) it backs off and re-LISTs from scratch —
/// the upstream resync path.
async fn watch_prefix(
    store: Arc<dyn StorageBackend>,
    resource: Resource,
    prefix: KeyPrefix,
    state: Arc<Mutex<ResolverState>>,
    tx: watch::Sender<Arc<ResolverState>>,
    shutdown: Shutdown,
) {
    loop {
        // Gap-free ordering: snapshot the revision FIRST, then LIST. The
        // LIST sees some revision >= it, and the watch replays from
        // `rev + 1` inclusive, so no write is lost (folds are idempotent,
        // so a replayed entry the LIST already saw is harmless).
        let rev = tokio::select! {
            _ = shutdown.cancelled() => return,
            res = store.current_revision() => match res {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(prefix = %prefix.as_path(), error = %e,
                        "service-plane revision read failed; retrying");
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
                        "service-plane LIST failed; retrying");
                    if backoff(&shutdown).await { return; }
                    continue;
                }
            },
        };
        {
            let mut st = state.lock().expect("service-plane state poisoned");
            replace_all(&mut st, resource, &entries);
            publish(&tx, &st);
        }
        let watch = tokio::select! {
            _ = shutdown.cancelled() => return,
            res = store.watch(&prefix, Some(rev + 1)) => match res {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(prefix = %prefix.as_path(), error = %e,
                        "service-plane WATCH failed; re-LISTing");
                    if backoff(&shutdown).await { return; }
                    continue;
                }
            },
        };
        tracing::info!(prefix = %prefix.as_path(), revision = rev,
            "service-plane reflector watching");
        let mut watch = watch;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                ev = watch.recv() => match ev {
                    Some(ev) => {
                        let mut st = state.lock().expect("service-plane state poisoned");
                        fold_event(&mut st, resource, &ev);
                        publish(&tx, &st);
                    }
                    None => break, // stream closed -> re-LIST
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
