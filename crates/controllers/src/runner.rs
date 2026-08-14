//! Controller-manager wiring (T3.1a, T3.2): leader election (Q18) + one
//! informer per resource + workqueues + reconciler workers.
//!
//! T3.1b additions (decision **Q20**): a Namespace informer/queue drives the
//! namespace lifecycle controller (drain + terminal delete); owner DELETE
//! events (Deployment/ReplicaSet/Service/StatefulSet/DaemonSet) spawn the
//! garbage collector (`gc::handle_owner_deleted`), with a 2s periodic
//! backstop sweep that also re-enqueues terminating namespaces.
//!
//! Event fan-out: Deployment events enqueue the Deployment; RS events enqueue
//! the RS **and** its owner Deployment; StatefulSet events enqueue the
//! StatefulSet; DaemonSet events enqueue the DaemonSet; Node events (the
//! DaemonSet placement source of truth) enqueue EVERY cached DaemonSet --
//! the node -> DaemonSet fan-out, the same list-and-enqueue shape as pod ->
//! Service below; ControllerRevision events enqueue the owner StatefulSet
//! (the revision is created by the STS reconciler itself -- this closes the
//! loop when a revision is created by another writer); Pod events enqueue
//! the owner RS, the owner StatefulSet (ordered rollout gates), the owner
//! DaemonSet, and every Service in the same namespace whose selector
//! matches the pod (this is the convergence path for Services created after
//! Pods); Service events enqueue the Service. Reconcilers read through the
//! [`Client`] (storage consistency) while caches drive the fan-out.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use storage::{Key, KeyPrefix, StorageBackend, WatchEvent};
use tokio::task::JoinHandle;

use crate::client::{Client, StorageClient};
use crate::controllers::{
    daemonset, deployment, endpoints, gc, namespace, replicaset, statefulset, Caches,
};
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
    statefulsets: Arc<WorkQueue>,
    daemonsets: Arc<WorkQueue>,
    services: Arc<WorkQueue>,
    namespaces: Arc<WorkQueue>,
}

#[derive(Clone, Copy)]
enum Kind {
    Deployment,
    ReplicaSet,
    StatefulSet,
    DaemonSet,
    Service,
    Namespace,
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
            statefulsets: Arc::new(WorkQueue::new()),
            daemonsets: Arc::new(WorkQueue::new()),
            services: Arc::new(WorkQueue::new()),
            namespaces: Arc::new(WorkQueue::new()),
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

    // Deployment events -> deployment queue (+ GC handoff on DELETE, Q20).
    let q = queues.deployments.clone();
    let gc_client = client.clone();
    let gc_caches = caches.clone();
    let dep_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if let Some(k) = event_object_key(ev) {
            q.add(k);
        }
        spawn_gc_on_delete(&gc_client, &gc_caches, ev);
    });

    // RS events -> RS queue + owner Deployment queue (+ GC handoff).
    let q_rs = queues.replicasets.clone();
    let q_dep = queues.deployments.clone();
    let gc_client = client.clone();
    let gc_caches = caches.clone();
    let rs_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        let Some(k) = event_object_key(ev) else {
            return;
        };
        q_rs.add(k.clone());
        enqueue_owner(&q_dep, ev, "Deployment");
        spawn_gc_on_delete(&gc_client, &gc_caches, ev);
    });

    // Pod events -> owner RS queue, owner StatefulSet queue (ordered rollout
    // gates: each created/deleted pod unblocks the next ordinal), owner
    // DaemonSet queue (placement/status counts) + every
    // same-namespace Service whose selector matches the pod.
    let q_rs = queues.replicasets.clone();
    let q_sts = queues.statefulsets.clone();
    let q_ds = queues.daemonsets.clone();
    let q_svc = queues.services.clone();
    let pod_caches = caches.clone();
    let pod_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        let Some(obj) = event_object(ev) else { return };
        enqueue_owner(&q_rs, ev, "ReplicaSet");
        enqueue_owner(&q_sts, ev, "StatefulSet");
        enqueue_owner(&q_ds, ev, "DaemonSet");
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

    // Service events -> service queue (+ GC handoff: Services own nothing
    // in the sweep universe, but the hook keeps the owner set uniform).
    let q = queues.services.clone();
    let gc_client = client.clone();
    let gc_caches = caches.clone();
    let svc_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if let Some(k) = event_object_key(ev) {
            q.add(k);
        }
        spawn_gc_on_delete(&gc_client, &gc_caches, ev);
    });

    // StatefulSet events (T3.1b) -> statefulset queue (+ GC handoff).
    let q = queues.statefulsets.clone();
    let gc_client = client.clone();
    let gc_caches = caches.clone();
    let sts_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if let Some(k) = event_object_key(ev) {
            q.add(k);
        }
        spawn_gc_on_delete(&gc_client, &gc_caches, ev);
    });

    // DaemonSet events (T3.1b) -> daemonset queue (deployment pattern, +
    // GC handoff).
    let q = queues.daemonsets.clone();
    let gc_client = client.clone();
    let gc_caches = caches.clone();
    let ds_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if let Some(k) = event_object_key(ev) {
            q.add(k);
        }
        spawn_gc_on_delete(&gc_client, &gc_caches, ev);
    });

    // Namespace events (T3.1b, Q20) -> namespace queue. Cluster-scoped: the
    // queue key is the bare object name (see the Kind::Namespace arm in
    // reconcile_key).
    let q = queues.namespaces.clone();
    let ns_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if let Some(k) = event_object_key(ev) {
            q.add(k);
        }
    });

    // Node events (T3.1b) -> EVERY cached DaemonSet. Nodes are the
    // DaemonSet placement source of truth, so any node create/delete or
    // label change must re-evaluate each DaemonSet (node -> DS fan-out;
    // the same list-and-enqueue shape as pod -> matching Services).
    let q_ds = queues.daemonsets.clone();
    let ds_caches = caches.clone();
    let node_handler: EventHandler = Arc::new(move |_ev: &WatchEvent| {
        for ds in ds_caches.daemonsets.list(None) {
            if let Some(name) = object::name(&ds) {
                let ns = object::namespace(&ds).unwrap_or("default");
                q_ds.add(format!("{ns}/{name}"));
            }
        }
    });

    // ControllerRevision events -> owner StatefulSet queue. No cache is
    // needed (the STS reconciler lists revisions through the client), but
    // `Informer::run` requires an ObjectStore: a dummy one, never read.
    let q_sts = queues.statefulsets.clone();
    let rev_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        enqueue_owner(&q_sts, ev, "StatefulSet");
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
            KeyPrefix::new("apps", "statefulsets", None),
            caches.statefulsets.clone(),
            sts_handler,
        ),
        (
            KeyPrefix::new("apps", "daemonsets", None),
            caches.daemonsets.clone(),
            ds_handler,
        ),
        (
            KeyPrefix::new("", "nodes", None),
            caches.nodes.clone(),
            node_handler,
        ),
        (
            KeyPrefix::new("apps", "controllerrevisions", None),
            // Dummy store (see the rev_handler note): filled, never read.
            Arc::new(crate::informer::ObjectStore::new()),
            rev_handler,
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
        (
            KeyPrefix::new("", "namespaces", None),
            caches.namespaces.clone(),
            ns_handler,
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
        (queues.statefulsets.clone(), Kind::StatefulSet),
        (queues.daemonsets.clone(), Kind::DaemonSet),
        (queues.services.clone(), Kind::Service),
        (queues.namespaces.clone(), Kind::Namespace),
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

    // GC backstop (T3.1b, Q20): owner-DELETE hooks drive the common path;
    // this 2s interval sweeps anything they missed (dangling ownerRefs
    // written outside the manager, missed events) and re-enqueues
    // terminating namespaces so the drain always progresses.
    let gc_client = client.clone();
    let gc_caches = caches.clone();
    let gc_ns_queue = queues.namespaces.clone();
    let gc_stop = stop.clone();
    handles.push(tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = gc_stop.cancelled() => return,
                _ = tick.tick() => {
                    if let Err(e) = gc::sweep(&gc_client, &gc_caches).await {
                        tracing::debug!(error = %e, "gc backstop sweep failed");
                    }
                    for ns in gc_caches.namespaces.list(None) {
                        if ns.pointer("/metadata/deletionTimestamp").is_some() {
                            if let Some(name) = object::name(&ns) {
                                gc_ns_queue.add(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }));
    handles
}

/// Fire-and-forget GC handoff on an owner DELETE event (Q20): the informer
/// applies the event to its cache BEFORE invoking the handler, so the owner
/// is already absent when the spawned cascade sweep runs. EventHandler is a
/// sync closure; spawning is the bridge.
fn spawn_gc_on_delete(client: &Arc<dyn Client>, caches: &Arc<Caches>, ev: &WatchEvent) {
    if let WatchEvent::Delete {
        prev: Some(prev), ..
    } = ev
    {
        let client = client.clone();
        let caches = caches.clone();
        let owner = prev.value.clone();
        tokio::spawn(async move {
            gc::handle_owner_deleted(&client, &caches, &owner).await;
        });
    }
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
    // Namespaces are cluster-scoped: their queue key is the BARE object
    // name (no `ns/` prefix), so they must not go through the namespaced
    // split below.
    if let Kind::Namespace = kind {
        let Some(ns) = caches
            .namespaces
            .list(None)
            .into_iter()
            .find(|n| object::name(n) == Some(key))
        else {
            return Ok(()); // deleted from cache: nothing to do
        };
        return namespace::reconcile(client, &ns).await;
    }
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
        Kind::StatefulSet => {
            let Some(sts) = caches.statefulsets.get(ns, name) else {
                return Ok(());
            };
            statefulset::reconcile(client, &sts).await
        }
        Kind::DaemonSet => {
            let Some(ds) = caches.daemonsets.get(ns, name) else {
                return Ok(());
            };
            daemonset::reconcile(client, &ds).await
        }
        Kind::Service => {
            let Some(svc) = caches.services.get(ns, name) else {
                return Ok(());
            };
            let ep_key = Key::new("", "endpoints", ns, name);
            let existing = client.get(&ep_key).await?;
            endpoints::reconcile(client, &svc, existing.as_ref()).await
        }
        // Handled by the cluster-scoped early return above; the namespaced
        // split is meaningless for a bare-name key.
        Kind::Namespace => Ok(()),
    }
}
