//! Controller-set informer fan-out wiring (T3.1a/T3.1b + T7.3, decision
//! **Q35**): one informer per resource feeding workqueues + workers.
//!
//! Extracted verbatim from `runner` (A5 split; code motion only). The
//! contract every row of the informer table honors:
//!
//! * an event enqueues the object's own key (`ns/name`, cluster-scoped
//!   bare name) unless its handler implements a wider fan-out;
//! * child objects enqueue their controller OWNER (RS->Deployment,
//!   Pod->RS/StatefulSet/DaemonSet/Workflow, Workflow->CronWorkflow,
//!   ControllerRevision->StatefulSet, Agent->Workflow) so a child
//!   transition drives the parent reconciler;
//! * owner-DELETE events hand off to the garbage collector (Q20) via
//!   [`spawn_gc_on_delete`], with the 2s backstop sweep below.
//!
//! Reconcilers read through the [`Client`] (storage consistency) while
//! caches drive the fan-out. T7.3/A5: Agent events (init-pro.io/v1
//! children of Workflows) enqueue only the owning Workflow -- the
//! scheduler owns Agent binding, so no Agent queue/worker exists.

use std::sync::Arc;
use std::time::Duration;

use storage::{KeyPrefix, WatchEvent};
use tokio::task::JoinHandle;

use crate::client::Client;
use crate::controllers::{gc, node_cidr, Caches};
use crate::informer::{event_object, event_object_key, EventHandler, Informer};
use crate::leaderelection::NowFn;
use crate::object;
use crate::runner::{Kind, Queues};
use crate::runner_worker::{
    enqueue_owner, spawn_gc_on_delete, spawn_worker, spawn_worker_with_cluster_cidr,
};
use crate::stop::Stop;

pub(crate) fn spawn_set(
    client: &Arc<dyn Client>,
    caches: &Arc<Caches>,
    queues: &Arc<Queues>,
    stop: Stop,
    now: NowFn,
    cluster_cidr: Arc<Option<node_cidr::ClusterCidr>>,
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
    // DaemonSet queue (placement/status counts), owner Workflow queue
    // (T7.3 Scope B: step phase transitions drive the node status) + every
    // same-namespace Service whose selector matches the pod.
    let q_rs = queues.replicasets.clone();
    let q_sts = queues.statefulsets.clone();
    let q_ds = queues.daemonsets.clone();
    let q_wf = queues.workflows.clone();
    let q_svc = queues.services.clone();
    let pod_caches = caches.clone();
    let pod_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        let Some(obj) = event_object(ev) else { return };
        enqueue_owner(&q_rs, ev, "ReplicaSet");
        enqueue_owner(&q_sts, ev, "StatefulSet");
        enqueue_owner(&q_ds, ev, "DaemonSet");
        enqueue_owner(&q_wf, ev, "Workflow");
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

    // Agent events (A5, Q35) -> owner Workflow queue + GC handoff
    // (deleting a Workflow cascades to its Agent children). The
    // pod_handler slice relevant to Workflows, minus the Service matching:
    // NO Agent queue/worker exists -- the scheduler owns Agent binding, and
    // the owning Workflow's 5s resync (WORKFLOW_RESYNC) re-aggregates
    // child phases.
    let q_wf = queues.workflows.clone();
    let gc_client = client.clone();
    let gc_caches = caches.clone();
    let agent_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        enqueue_owner(&q_wf, ev, "Workflow");
        spawn_gc_on_delete(&gc_client, &gc_caches, ev);
    });

    // Workflow events (T7.3 Scope B, Q30) -> workflow queue (+ GC handoff:
    // deleting a Workflow cascades to its step pods). Child events also
    // re-enqueue the owning CronWorkflow (T7.3 Scope C): a terminal child
    // promptly releases a Forbid policy on the parent's next tick.
    let q_wf = queues.workflows.clone();
    let q_cwf = queues.cronworkflows.clone();
    let gc_client = client.clone();
    let gc_caches = caches.clone();
    let wf_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if let Some(k) = event_object_key(ev) {
            q_wf.add(k);
        }
        enqueue_owner(&q_cwf, ev, "CronWorkflow");
        spawn_gc_on_delete(&gc_client, &gc_caches, ev);
    });

    // CronWorkflow events (T7.3 Scope C, Q30) -> cronworkflow queue (+
    // GC handoff: deleting a CronWorkflow cascades to its child
    // Workflows).
    let q = queues.cronworkflows.clone();
    let gc_client = client.clone();
    let gc_caches = caches.clone();
    let cwf_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if let Some(k) = event_object_key(ev) {
            q.add(k);
        }
        spawn_gc_on_delete(&gc_client, &gc_caches, ev);
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

    // Node events (T4.3 Scope A, Q34) -> podCIDR allocator queue. Bare
    // node name (cluster-scoped); a SECOND informer on the same Nodes
    // prefix drives this queue with its own handler, so DS fan-out and
    // CIDR allocation stay independent. Disabled when `cluster_cidr` is
    // None: the handler still enqueues (a WorkQueue add is ~free) and the
    // worker short-circuits, keeping the wiring unconditional.
    let q_nc = queues.node_cidrs.clone();
    let node_cidr_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if let Some(k) = event_object_key(ev) {
            q_nc.add(k);
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
            // Second Nodes informer (T4.3 Scope A): podCIDR allocator
            // events. Dummy store, never read -- `allocate` reads the
            // Node fresh through the client so its CAS base is the
            // current revision, not a cached one.
            KeyPrefix::new("", "nodes", None),
            Arc::new(crate::informer::ObjectStore::new()),
            node_cidr_handler,
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
            KeyPrefix::new("init-pro.io", "agents", None),
            caches.agents.clone(),
            agent_handler,
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
        (
            KeyPrefix::new("argoproj.io", "workflows", None),
            caches.workflows.clone(),
            wf_handler,
        ),
        (
            KeyPrefix::new("argoproj.io", "cronworkflows", None),
            caches.cronworkflows.clone(),
            cwf_handler,
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
        (queues.cronworkflows.clone(), Kind::CronWorkflow),
        (queues.deployments.clone(), Kind::Deployment),
        (queues.replicasets.clone(), Kind::ReplicaSet),
        (queues.statefulsets.clone(), Kind::StatefulSet),
        (queues.daemonsets.clone(), Kind::DaemonSet),
        (queues.services.clone(), Kind::Service),
        (queues.namespaces.clone(), Kind::Namespace),
        (queues.workflows.clone(), Kind::Workflow),
    ] {
        for _ in 0..2 {
            handles.push(spawn_worker(
                client,
                caches,
                &queue,
                kind,
                &stop,
                now.clone(),
            ));
        }
    }

    // Node podCIDR allocator (T4.3 Scope A, Q34): ONE worker on purpose.
    // Serial processing is the concurrency-correctness mechanism: LIST ->
    // pick first free -> CAS across two workers could double-assign the
    // same free /24 to two Nodes (the CAS only guards per-object writes,
    // not the inter-node allocation decision).
    {
        let queue = queues.node_cidrs.clone();
        let cluster_cidr = cluster_cidr.clone();
        handles.push(spawn_worker_with_cluster_cidr(
            client,
            caches,
            &queue,
            Kind::NodeCidr,
            cluster_cidr,
            &stop,
            now.clone(),
        ));
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
