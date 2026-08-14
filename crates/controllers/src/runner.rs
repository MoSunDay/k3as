//! Controller-manager wiring (T3.1a, T3.2): leader election (Q18) + one
//! informer per resource + workqueues + reconciler workers.
//!
//! Event fan-out: Deployment events enqueue the Deployment; RS events enqueue
//! the RS **and** its owner Deployment; Pod events enqueue the owner RS and
//! every Service in the same namespace whose selector matches the pod (this
//! is the convergence path for Services created after Pods); Service events
//! enqueue the Service. Reconcilers read through the [`Client`] (storage
//! consistency) while caches drive the fan-out.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use storage::{Key, KeyPrefix, StorageBackend, WatchEvent};
use tokio::task::JoinHandle;

use crate::client::{Client, StorageClient};
use crate::controllers::{deployment, endpoints, replicaset, Caches};
use crate::error::ControllerError;
use crate::informer::{event_object, event_object_key, EventHandler, Informer};
use crate::leaderelection::{LeaderElector, LeaseConfig, NowFn};
use crate::object::{self, controller_of};
use crate::stop::Stop;
use crate::workqueue::WorkQueue;

/// Wiring entry point; owns no long-lived mutable state itself.
pub struct ControllerManager;

struct Queues {
    deployments: Arc<WorkQueue>,
    replicasets: Arc<WorkQueue>,
    services: Arc<WorkQueue>,
}

#[derive(Clone, Copy)]
enum Kind {
    Deployment,
    ReplicaSet,
    Service,
}

impl ControllerManager {
    /// Spawn the full controller manager against a shared storage backend.
    /// Returns the joinable top-level tasks (leader-election loop +
    /// supervisor). The actual controller set is re-spawned/aborted by the
    /// supervisor on leadership transitions.
    pub fn spawn(store: Arc<dyn StorageBackend>, stop: Stop) -> Vec<JoinHandle<()>> {
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        let now: NowFn = Arc::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
        let cfg = LeaseConfig {
            namespace: "kube-system".into(),
            name: "init-pro-controller-manager".into(),
            identity: format!("init-pro-{}", crate::id::rand_suffix(5)),
            lease_duration: 30,
            retry_period: 500,
            now: now.clone(),
        };
        let (leader_handle, mut leader_rx) =
            LeaderElector::new(client.clone(), cfg).spawn(stop.clone());

        let caches = Arc::new(Caches::new());
        let queues = Arc::new(Queues {
            deployments: Arc::new(WorkQueue::new()),
            replicasets: Arc::new(WorkQueue::new()),
            services: Arc::new(WorkQueue::new()),
        });

        let supervisor = tokio::spawn(async move {
            loop {
                if !*leader_rx.borrow_and_update() {
                    tokio::select! {
                        _ = stop.cancelled() => return,
                        r = leader_rx.changed() => {
                            if r.is_err() {
                                return; // elector task gone
                            }
                        }
                    }
                    continue;
                }
                tracing::info!("acquired leadership; starting controller set");
                bootstrap_namespaces(&client).await;
                let set = spawn_set(&client, &caches, &queues, stop.clone(), now.clone());
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => {
                            abort_set(&set);
                            return;
                        }
                        r = leader_rx.changed() => {
                            if r.is_err() {
                                abort_set(&set);
                                return;
                            }
                            if !*leader_rx.borrow_and_update() {
                                tracing::info!("lost leadership; stopping controller set");
                                // Accept: aborting workers mid-processing can
                                // leak an in-flight key; the whole set is
                                // rebuilt on the next acquisition.
                                abort_set(&set);
                                break;
                            }
                        }
                    }
                }
            }
        });
        vec![leader_handle, supervisor]
    }
}

async fn bootstrap_namespaces(client: &Arc<dyn Client>) {
    for ns in ["default", "kube-system", "kube-public", "kube-node-lease"] {
        let key = Key::new("", "namespaces", "", ns);
        let obj = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": ns},
        });
        match client.create(&key, obj).await {
            Ok(_) => tracing::debug!(namespace = ns, "bootstrapped namespace"),
            Err(e) if e.is_already_exists() => {}
            Err(e) => tracing::warn!(namespace = ns, error = %e, "namespace bootstrap failed"),
        }
    }
}

fn abort_set(set: &[JoinHandle<()>]) {
    for h in set {
        h.abort();
    }
}

fn spawn_set(
    client: &Arc<dyn Client>,
    caches: &Arc<Caches>,
    queues: &Arc<Queues>,
    stop: Stop,
    now: NowFn,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();

    // Deployment events -> deployment queue.
    let q = queues.deployments.clone();
    let dep_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if let Some(k) = event_object_key(ev) {
            q.add(k);
        }
    });

    // RS events -> RS queue + owner Deployment queue.
    let q_rs = queues.replicasets.clone();
    let q_dep = queues.deployments.clone();
    let rs_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        let Some(k) = event_object_key(ev) else {
            return;
        };
        q_rs.add(k.clone());
        enqueue_owner(&q_dep, ev, "Deployment");
    });

    // Pod events -> owner RS queue + every same-namespace Service whose
    // selector matches the pod (late-Service convergence).
    let q_rs = queues.replicasets.clone();
    let q_svc = queues.services.clone();
    let pod_caches = caches.clone();
    let pod_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        let Some(obj) = event_object(ev) else { return };
        enqueue_owner(&q_rs, ev, "ReplicaSet");
        let ns = object::namespace(&obj).unwrap_or("default");
        for svc in pod_caches.services.list(Some(ns)) {
            if let Some(sel) = svc.pointer("/spec/selector") {
                if object::selector_matches(&obj, sel) {
                    if let Some(name) = object::name(&svc) {
                        q_svc.add(format!("{ns}/{name}"));
                    }
                }
            }
        }
    });

    // Service events -> service queue.
    let q = queues.services.clone();
    let svc_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if let Some(k) = event_object_key(ev) {
            q.add(k);
        }
    });

    let sets: Vec<(KeyPrefix, Arc<crate::informer::ObjectStore>, EventHandler)> = vec![
        (
            KeyPrefix::new("apps", "deployments", None),
            caches.deployments.clone(),
            dep_handler,
        ),
        (
            KeyPrefix::new("apps", "replicasets", None),
            caches.replicasets.clone(),
            rs_handler,
        ),
        (
            KeyPrefix::new("", "pods", None),
            caches.pods.clone(),
            pod_handler,
        ),
        (
            KeyPrefix::new("", "services", None),
            caches.services.clone(),
            svc_handler,
        ),
    ];
    for (prefix, store, handler) in sets {
        let informer = Informer::new(prefix);
        let client = client.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            informer.run(client, store, handler, stop).await;
        }));
    }

    for (queue, kind) in [
        (queues.deployments.clone(), Kind::Deployment),
        (queues.replicasets.clone(), Kind::ReplicaSet),
        (queues.services.clone(), Kind::Service),
    ] {
        for _ in 0..2 {
            spawn_worker(
                client,
                caches,
                &queue,
                kind,
                &stop,
                now.clone(),
                &mut handles,
            );
        }
    }
    handles
}

/// Enqueue the controller-owner of the event object (if it is a `kind`).
fn enqueue_owner(q: &Arc<WorkQueue>, ev: &WatchEvent, kind: &str) {
    let Some(obj) = event_object(ev) else { return };
    let Some(owner) = controller_of(&obj) else {
        return;
    };
    if owner.get("kind").and_then(|v| v.as_str()) != Some(kind) {
        return;
    }
    if let Some(name) = owner.get("name").and_then(|v| v.as_str()) {
        let ns = object::namespace(&obj).unwrap_or("default");
        q.add(format!("{ns}/{name}"));
    }
}

fn spawn_worker(
    client: &Arc<dyn Client>,
    caches: &Arc<Caches>,
    queue: &Arc<WorkQueue>,
    kind: Kind,
    stop: &Stop,
    now: NowFn,
    out: &mut Vec<JoinHandle<()>>,
) {
    let client = client.clone();
    let caches = caches.clone();
    let queue = queue.clone();
    let stop = stop.clone();
    out.push(tokio::spawn(async move {
        loop {
            // Workers must race `next()` against `stop`: the queue's senders
            // are never dropped, `next` alone would never return None.
            let key = tokio::select! {
                _ = stop.cancelled() => break,
                k = queue.next() => match k {
                    Some(k) => k,
                    None => break,
                },
            };
            match reconcile_key(&client, &caches, &queue, kind, &key, &now).await {
                Ok(()) => {
                    queue.forget(&key);
                    queue.done(&key);
                }
                Err(e) if e.is_not_found() || e.is_already_exists() => {
                    // Vanished object / duplicate create: idempotent no-op.
                    tracing::debug!(key = %key, error = %e, "benign reconcile error");
                    queue.done(&key);
                }
                Err(e) if e.is_conflict() => {
                    // CAS lost: retry against the refreshed cache. Dropping
                    // would stall a quiesced object permanently.
                    tracing::debug!(key = %key, error = %e, "reconcile conflict; retrying");
                    queue.add_rate_limited(key.clone());
                    queue.done(&key);
                }
                Err(e) => {
                    tracing::warn!(key = %key, error = %e, "reconcile failed; rate-limited requeue");
                    queue.add_rate_limited(key.clone());
                    queue.done(&key);
                }
            }
        }
    }));
}

/// Deployment resync period (T3.1b): progress-deadline evaluation is
/// time-based, so quiesced (event-less) rollouts still need periodic
/// reconciles. A no-op reconcile writes nothing, so the anti-oscillation
/// gate holds.
const DEPLOYMENT_RESYNC: Duration = Duration::from_secs(1);

async fn reconcile_key(
    client: &Arc<dyn Client>,
    caches: &Caches,
    queue: &Arc<WorkQueue>,
    kind: Kind,
    key: &str,
    now: &NowFn,
) -> Result<(), ControllerError> {
    let (ns, name) = match key.split_once('/') {
        Some((ns, name)) => (ns, name),
        None => ("default", key),
    };
    match kind {
        Kind::Deployment => {
            let Some(dep) = caches.deployments.get(ns, name) else {
                return Ok(()); // deleted from cache: nothing to do
            };
            deployment::reconcile(client, &dep, now()).await?;
            // Resync (see DEPLOYMENT_RESYNC); the loop self-terminates once
            // the object leaves the cache.
            queue.add_after(key.to_string(), DEPLOYMENT_RESYNC);
            Ok(())
        }
        Kind::ReplicaSet => {
            let Some(rs) = caches.replicasets.get(ns, name) else {
                return Ok(());
            };
            replicaset::reconcile(client, &rs).await
        }
        Kind::Service => {
            let Some(svc) = caches.services.get(ns, name) else {
                return Ok(());
            };
            let ep_key = Key::new("", "endpoints", ns, name);
            let existing = client.get(&ep_key).await?;
            endpoints::reconcile(client, &svc, existing.as_ref()).await
        }
    }
}
