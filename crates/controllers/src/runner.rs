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
//! Pods); Service events enqueue the Service. Workflow events (T7.3)
//! enqueue the Workflow and its owning CronWorkflow (child terminal
//! transitions promptly re-evaluate Forbid/Replace); CronWorkflow events
//! (T7.3 Scope C) enqueue the CronWorkflow. Reconcilers read through the
//! [`Client`] (storage consistency) while caches drive the fan-out.
//!
//! T4.3 Scope A (decision **Q34**): a second Nodes informer feeds a
//! single-worker `NodeCidr` queue. Enabled via `--cluster-cidr`
//! ([`ManagerConfig::cluster_cidr`]), it allocates `spec.podCIDR` /
//! `spec.podCIDRs` to Nodes that lack one (first free /24, CAS-written) --
//! the kube-controller-manager `--allocate-node-cidrs` loop. One worker
//! by design: the LIST -> first-free -> CAS decision is serial so a
//! subnet is never double-assigned to two Nodes.
//!
//! A5 split (T7.3, decision **Q35**): the informer fan-out table lives in
//! [`runner_wiring`] and the worker loop + per-kind dispatch in
//! [`runner_worker`]; this module keeps the leadership supervisor, the
//! manager config, the queue/kind tables and namespace bootstrap. Agents
//! (`init-pro.io/v1`, Workflow step children) ride the Workflow queue:
//! their events enqueue the owning Workflow, which re-aggregates phases on
//! the 5s resync.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use storage::{Key, StorageBackend};
use tokio::task::JoinHandle;

use crate::client::{Client, StorageClient};
use crate::controllers::{node_cidr, Caches};
use crate::leaderelection::{LeaderElector, LeaseConfig, NowFn};
use crate::stop::Stop;
use crate::workqueue::WorkQueue;

/// Wiring entry point; owns no long-lived mutable state itself.
pub struct ControllerManager;

/// Optional manager configuration (T4.3 Scope A). `cluster_cidr` enables
/// the node podCIDR allocator; `None` (the `spawn` default) keeps the
/// manager byte-identical to the pre-T4.3 controller set.
#[derive(Debug, Clone, Default)]
pub struct ManagerConfig {
    /// `--cluster-cidr`: when set, Nodes without `spec.podCIDR` are
    /// allocated the first free `/24` inside it (decision **Q34**).
    pub cluster_cidr: Option<String>,
    /// The anyhome NFS endpoint (`host:port`, **T-anyhome**, decision
    /// **Q36**): when set, Workflow step pods get the `/workspace` shared
    /// volume auto-mounted from it; `None` (the `spawn` default) leaves
    /// step pods byte-identical to the pre-Q36 builder output.
    pub workspace_nfs: Option<String>,
}

pub(crate) struct Queues {
    pub(crate) cronworkflows: Arc<WorkQueue>,
    pub(crate) deployments: Arc<WorkQueue>,
    pub(crate) replicasets: Arc<WorkQueue>,
    pub(crate) statefulsets: Arc<WorkQueue>,
    pub(crate) daemonsets: Arc<WorkQueue>,
    pub(crate) services: Arc<WorkQueue>,
    pub(crate) namespaces: Arc<WorkQueue>,
    pub(crate) workflows: Arc<WorkQueue>,
    /// Node podCIDR allocator (T4.3 Scope A); single worker by design.
    pub(crate) node_cidrs: Arc<WorkQueue>,
}

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    CronWorkflow,
    Deployment,
    ReplicaSet,
    StatefulSet,
    DaemonSet,
    Service,
    Namespace,
    Workflow,
    NodeCidr,
}

impl ControllerManager {
    /// Spawn the full controller manager against a shared storage backend.
    /// Returns the joinable top-level tasks (leader-election loop +
    /// supervisor). The actual controller set is re-spawned/aborted by the
    /// supervisor on leadership transitions.
    pub fn spawn(store: Arc<dyn StorageBackend>, stop: Stop) -> Vec<JoinHandle<()>> {
        ControllerManager::spawn_with(store, stop, ManagerConfig::default())
    }

    /// [`ControllerManager::spawn`] with explicit configuration
    /// (T4.3 Scope A: `cluster_cidr` turns on the podCIDR allocator).
    pub fn spawn_with(
        store: Arc<dyn StorageBackend>,
        stop: Stop,
        cfg: ManagerConfig,
    ) -> Vec<JoinHandle<()>> {
        let cluster_cidr: Arc<Option<node_cidr::ClusterCidr>> = Arc::new(
            cfg.cluster_cidr
                .as_deref()
                .and_then(node_cidr::parse_cidr),
        );
        if let (Some(flag), None) = (cfg.cluster_cidr.as_deref(), cluster_cidr.as_ref()) {
            tracing::warn!(cluster_cidr = flag, "invalid --cluster-cidr; podCIDR allocation disabled");
        }
        // T-anyhome (Q36): the Workflow controller's `/workspace` auto-mount
        // endpoint, threaded to the worker dispatch next to cluster_cidr.
        let workspace_nfs: Arc<Option<String>> = Arc::new(cfg.workspace_nfs.clone());
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
            cronworkflows: Arc::new(WorkQueue::new()),
            deployments: Arc::new(WorkQueue::new()),
            replicasets: Arc::new(WorkQueue::new()),
            statefulsets: Arc::new(WorkQueue::new()),
            daemonsets: Arc::new(WorkQueue::new()),
            services: Arc::new(WorkQueue::new()),
            namespaces: Arc::new(WorkQueue::new()),
            workflows: Arc::new(WorkQueue::new()),
            node_cidrs: Arc::new(WorkQueue::new()),
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
                let set = crate::runner_wiring::spawn_set(
                    &client,
                    &caches,
                    &queues,
                    stop.clone(),
                    now.clone(),
                    cluster_cidr.clone(),
                    workspace_nfs.clone(),
                );
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
