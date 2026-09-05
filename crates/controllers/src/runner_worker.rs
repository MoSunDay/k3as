//! Reconciler workers + dispatch (T3.1a/T3.1b + T4.3 Scope A, decision
//! **Q35**): one worker per workqueue draining keys into [`reconcile_key`].
//!
//! Extracted verbatim from `runner` (A5 split; code motion only):
//!
//! * [`enqueue_owner`] -- the shared child -> owner enqueuer used by the
//!   informer handlers (Agent -> Workflow included, A5);
//! * [`spawn_gc_on_delete`] -- the fire-and-forget GC handoff on owner
//!   DELETE events (Q20);
//! * [`spawn_worker`] / [`spawn_worker_with_cluster_cidr`] -- the worker
//!   loop: race `next()` against stop, reconcile, then classify the error
//!   (benign not-found/already-exists, Conflict -> rate-limited retry,
//!   anything else -> rate-limited requeue);
//! * the resync consts + [`reconcile_key`] -- the per-kind dispatch, where
//!   self-terminating resyncs (`add_after`) keep time-driven controllers
//!   (Deployment progress deadline, Workflow/CronWorkflow schedules)
//!   progressing without events.

use std::sync::Arc;
use std::time::Duration;

use storage::{Key, WatchEvent};
use tokio::task::JoinHandle;

use crate::client::Client;
use crate::controllers::{
    cronworkflow, daemonset, deployment, endpoints, gc, namespace, node_cidr, replicaset,
    statefulset, workflow, Caches,
};
use crate::error::ControllerError;
use crate::informer::event_object;
use crate::leaderelection::NowFn;
use crate::object::{self, controller_of};
use crate::runner::Kind;
use crate::stop::Stop;
use crate::workqueue::WorkQueue;

/// Fire-and-forget GC handoff on an owner DELETE event (Q20): the informer
/// applies the event to its cache BEFORE invoking the handler, so the owner
/// is already absent when the spawned cascade sweep runs. EventHandler is a
/// sync closure; spawning is the bridge.
pub(crate) fn spawn_gc_on_delete(client: &Arc<dyn Client>, caches: &Arc<Caches>, ev: &WatchEvent) {
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
pub(crate) fn enqueue_owner(q: &Arc<WorkQueue>, ev: &WatchEvent, kind: &str) {
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

pub(crate) fn spawn_worker(
    client: &Arc<dyn Client>,
    caches: &Arc<Caches>,
    queue: &Arc<WorkQueue>,
    kind: Kind,
    workspace_nfs: Arc<Option<String>>,
    stop: &Stop,
    now: NowFn,
) -> JoinHandle<()> {
    spawn_worker_with_cluster_cidr(
        client,
        caches,
        queue,
        kind,
        Arc::new(None),
        workspace_nfs,
        stop,
        now,
    )
}

/// [`spawn_worker`] variant carrying the podCIDR allocator config
/// (T4.3 Scope A, Q34: `Kind::NodeCidr` needs the parsed `--cluster-cidr`)
/// plus the workspace NFS endpoint (T-anyhome, Q36: `Kind::Workflow` needs
/// it for the step-pod `/workspace` auto-mount).
#[allow(clippy::too_many_arguments)] // 8 = over the lint's ceiling; all live config
pub(crate) fn spawn_worker_with_cluster_cidr(
    client: &Arc<dyn Client>,
    caches: &Arc<Caches>,
    queue: &Arc<WorkQueue>,
    kind: Kind,
    cluster_cidr: Arc<Option<node_cidr::ClusterCidr>>,
    workspace_nfs: Arc<Option<String>>,
    stop: &Stop,
    now: NowFn,
) -> JoinHandle<()> {
    let client = client.clone();
    let caches = caches.clone();
    let queue = queue.clone();
    let stop = stop.clone();
    tokio::spawn(async move {
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
            match reconcile_key(
                &client,
                &caches,
                &queue,
                kind,
                &cluster_cidr,
                &workspace_nfs,
                &key,
                &now,
            )
            .await
            {
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
    })
}

/// Deployment resync period (T3.1b): progress-deadline evaluation is
/// time-based, so quiesced (event-less) rollouts still need periodic
/// reconciles. A no-op reconcile writes nothing, so the anti-oscillation
/// gate holds.
const DEPLOYMENT_RESYNC: Duration = Duration::from_secs(1);
/// Workflow resync backstop (T7.3 Scope B): pod events drive the normal
/// loop; this keeps Pending/edge cases from stalling indefinitely.
const WORKFLOW_RESYNC: Duration = Duration::from_secs(5);
/// CronWorkflow resync (T7.3 Scope C): schedules fire on wall-clock time
/// with no event to observe, so the queue re-enqueues periodically (the
/// same shape as WORKFLOW_RESYNC).
const CRONWORKFLOW_RESYNC: Duration = Duration::from_secs(5);

/// Per-kind dispatch (see the module doc). Grows one config parameter per
/// plumb-through (Q34 podCIDR, T-anyhome/Q36 workspace), hence the allow.
#[allow(clippy::too_many_arguments)] // 8 = over the lint's ceiling; all live config
pub(crate) async fn reconcile_key(
    client: &Arc<dyn Client>,
    caches: &Caches,
    queue: &Arc<WorkQueue>,
    kind: Kind,
    cluster_cidr: &Option<node_cidr::ClusterCidr>,
    // T-anyhome (Q36): Some(addr) = Workflow step pods get the /workspace
    // NFS volume auto-mounted from the anyhome endpoint (and Agent children
    // get the workspace contract annotation).
    workspace_nfs: &Option<String>,
    key: &str,
    now: &NowFn,
) -> Result<(), ControllerError> {
    // Node podCIDR allocation (T4.3 Scope A, Q34) is cluster-scoped: its
    // queue key is the BARE node name. The Node is read fresh through the
    // client (this kind has no cache) so the CAS base is the current
    // revision; a concurrent node patch surfaces as Conflict -> requeue.
    if let Kind::NodeCidr = kind {
        let Some(cluster) = cluster_cidr else {
            return Ok(()); // allocator disabled: no --cluster-cidr
        };
        return match client.get(&Key::new("", "nodes", "", key)).await? {
            Some(node) => node_cidr::allocate(client, cluster, &node)
                .await
                .map(|_| ()),
            None => Ok(()), // node deleted: nothing to allocate
        };
    }
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
        Kind::Namespace | Kind::NodeCidr => Ok(()),
        Kind::Workflow => {
            let Some(wf) = caches.workflows.get(ns, name) else {
                return Ok(()); // deleted from cache: nothing to do
            };
            workflow::reconcile(client, &wf, workspace_nfs).await?;
            // Resync backstop (see WORKFLOW_RESYNC); the loop self-
            // terminates once the object leaves the cache.
            queue.add_after(key.to_string(), WORKFLOW_RESYNC);
            Ok(())
        }
        Kind::CronWorkflow => {
            let Some(cwf) = caches.cronworkflows.get(ns, name) else {
                return Ok(()); // deleted from cache: nothing to do
            };
            cronworkflow::reconcile(client, &cwf).await?;
            // Resync (see CRONWORKFLOW_RESYNC): the next due tick arrives
            // by wall clock alone; the loop self-terminates once the
            // object leaves the cache.
            queue.add_after(key.to_string(), CRONWORKFLOW_RESYNC);
            Ok(())
        }
    }
}
