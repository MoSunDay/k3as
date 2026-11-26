//! Reconciler workers + dispatch (T3.1a/T3.1b + T4.3 Scope A, decision
//! **Q35**): one worker per workqueue draining keys into [`reconcile_key`].
//!
//! Extracted verbatim from `runner` (A5 split; code motion only):
//!
//! * [`enqueue_owner`] -- the shared child -> owner enqueuer used by the
//!   informer handlers (Agent -> Workflow included, A5);
//! * [`spawn_gc_on_delete`] -- the fire-and-forget GC handoff on owner
//!   DELETE events (Q20);
//! * [`spawn_worker`] -- the worker loop: race `next()` against stop,
//!   reconcile, then classify the error (benign not-found/already-exists,
//!   Conflict -> rate-limited retry, anything else -> rate-limited
//!   requeue); every per-kind config rides one [`WorkerConfig`] (Q34/Q36/
//!   Q41/Q56) so the worker arity stays fixed;
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
    cronworkflow, daemonset, deployment, endpoints, gc, namespace, node_cidr, pipeline, replicaset,
    service_lb, statefulset, workflow, Caches,
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

/// Live manager config threaded into every worker (Q34 podCIDR, Q36
/// workspace NFS, Q41 ServiceLB, Q56 pipeline). Cloned per worker; the
/// inner values are cheap `Arc`/`Option` handles.
#[derive(Clone, Default)]
pub(crate) struct WorkerConfig {
    /// Parsed `--cluster-cidr`; `None` = podCIDR allocator disabled (Q34).
    pub(crate) cluster_cidr: Arc<Option<node_cidr::ClusterCidr>>,
    /// anyhome NFS endpoint: Workflow step pods get `/workspace` (Q36).
    pub(crate) workspace_nfs: Arc<Option<String>>,
    /// Pipeline plane config; `None` = pipeline controllers disabled (Q56).
    pub(crate) pipeline: Arc<Option<pipeline::PipelineConfig>>,
}

/// Worker loop for one queue. `WorkerConfig` carries every per-kind knob so
/// the signature stays stable as controllers are added (Q34/Q36/Q41/Q56).
pub(crate) fn spawn_worker(
    client: &Arc<dyn Client>,
    caches: &Arc<Caches>,
    queue: &Arc<WorkQueue>,
    kind: Kind,
    cfg: WorkerConfig,
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
            // A3: the reconcile runs in a supervised task. A panic in a
            // reconciler used to kill this worker permanently (and with the
            // single-worker kinds -- NodeCidr, ServiceLb, Build -- the whole
            // controller silently died). Now the panic is contained, logged
            // and the key requeued with backoff.
            let task = {
                let client = client.clone();
                let caches = caches.clone();
                let queue = queue.clone();
                let cfg = cfg.clone();
                let now = now.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    reconcile_key(&client, &caches, &queue, kind, &cfg, &key, &now).await
                })
            };
            let outcome = match task.await {
                Ok(outcome) => outcome,
                Err(join) if join.is_panic() => {
                    tracing::error!(
                        key = %key,
                        kind = ?kind,
                        "reconcile panicked; worker survived and requeued the key"
                    );
                    queue.add_rate_limited(key.clone());
                    queue.done(&key);
                    continue;
                }
                Err(join) => {
                    // Runtime shutdown / task abort: stop draining.
                    tracing::debug!(key = %key, error = %join, "reconcile task dropped");
                    break;
                }
            };
            match outcome {
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
/// ServicePipeline resync (T8.3, Q56): promotion waits on the target
/// Deployment's rollout (and on a Build child the cache may not have seen
/// yet), so a quiesced pipeline still re-checks every 2s. The Builds
/// themselves get NO resync: the informer's initial LIST re-delivers
/// in-flight children, and a periodic kick could double-run a docker
/// build.
const PIPELINE_RESYNC: Duration = Duration::from_secs(2);

/// Per-kind dispatch (see the module doc). All per-kind config arrives
/// through [`WorkerConfig`] (Q34 podCIDR, Q36 workspace NFS, Q56 pipeline).
pub(crate) async fn reconcile_key(
    client: &Arc<dyn Client>,
    caches: &Caches,
    queue: &Arc<WorkQueue>,
    kind: Kind,
    cfg: &WorkerConfig,
    key: &str,
    now: &NowFn,
) -> Result<(), ControllerError> {
    // Node podCIDR allocation (T4.3 Scope A, Q34) is cluster-scoped: its
    // queue key is the BARE node name. The Node is read fresh through the
    // client (this kind has no cache) so the CAS base is the current
    // revision; a concurrent node patch surfaces as Conflict -> requeue.
    if let Kind::NodeCidr = kind {
        let Some(cluster) = cfg.cluster_cidr.as_ref() else {
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
        // ServiceLB announce (T4.3 Scope B / T5.6, Q41): the Service is
        // re-read FRESH through the client (the reconcile owns its CAS
        // base), so the cache is only the deleted-from-cache no-op gate.
        // The worker itself is only spawned when ServiceLB is enabled
        // (ManagerConfig::disable_servicelb, Q41).
        Kind::ServiceLb => {
            if caches.services.get(ns, name).is_none() {
                return Ok(()); // deleted from cache: nothing to announce
            }
            service_lb::reconcile(client, ns, name).await
        }
        // Handled by the cluster-scoped early return above; the namespaced
        // split is meaningless for a bare-name key.
        Kind::Namespace | Kind::NodeCidr => Ok(()),
        Kind::Workflow => {
            let Some(wf) = caches.workflows.get(ns, name) else {
                return Ok(()); // deleted from cache: nothing to do
            };
            workflow::reconcile(client, &wf, &cfg.workspace_nfs).await?;
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
        // CI/CD pipeline controller (T8.3, Q56): tags -> Build children ->
        // registry push -> Deployment image promotion. The 2s resync keeps
        // a quiesced pipeline re-checking its rollout / build child.
        Kind::ServicePipeline => {
            let Some(sp) = caches.servicepipelines.get(ns, name) else {
                return Ok(()); // deleted from cache: nothing to do
            };
            pipeline::reconcile(client, &sp, &cfg.pipeline).await?;
            queue.add_after(key.to_string(), PIPELINE_RESYNC);
            Ok(())
        }
        // Build phase machine (T8.3, Q56): one docker build + registry push
        // per child. No resync on purpose (see PIPELINE_RESYNC): a re-run of
        // a terminal Build writes nothing (the phase is immutable) and an
        // in-flight one is owned by the worker currently processing it.
        Kind::Build => {
            let Some(build) = caches.builds.get(ns, name) else {
                return Ok(());
            };
            let Some(pipeline_cfg) = cfg.pipeline.as_ref() else {
                return Ok(()); // pipeline plane disabled
            };
            pipeline::build::reconcile(client, &build, pipeline_cfg).await
        }
    }
}
