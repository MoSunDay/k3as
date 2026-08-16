//! Scheduler wiring (T3.2, Q23): the `ControllerManager::spawn` shape applied
//! to scheduling — in-process storage transport (**Q19**), Lease+CAS leader
//! election under `init-pro-scheduler` (**Q18**), pod + node + PVC
//! informers feeding one pending-pod [`WorkQueue`], and worker loops that
//! run [`crate::cycle::schedule_one`] then bind or mark unschedulable.
//!
//! Event fan-out (upstream parity): a pod event enqueues the pod **iff**
//! pending; a node event re-enqueues every cached pending pod (new capacity
//! must retry the fleet). Unschedulable pods are never re-enqueued by the
//! worker — only events or the 30 s backstop sweep — so a stuck cluster
//! cannot hot-loop (anti-oscillation, the Sprint-13 quiesce pattern).

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use storage::{KeyPrefix, StorageBackend, WatchEvent};
use tokio::task::JoinHandle;

use controllers::client::{Client, StorageClient};
use controllers::id::rand_suffix;
use controllers::informer::{event_object, event_object_key, EventHandler, Informer, ObjectStore};
use controllers::leaderelection::{LeaderElector, LeaseConfig};
use controllers::object;
use controllers::stop::Stop;
use controllers::workqueue::WorkQueue;

use crate::bind;
use crate::cycle::schedule_one;
use crate::extender::{ExtenderConfig, ExtenderSet};
use crate::plugin::{default_filters, default_scores, is_pending, Filter, Score};

/// Default scheduler name (`spec.schedulerName` values we claim).
pub const DEFAULT_SCHEDULER_NAME: &str = "default-scheduler";
/// Backstop resync: re-enqueue pending pods even with no events (missed
/// watch edges, storage restarts).
const RESYNC: Duration = Duration::from_secs(30);
/// Workers per queue (same shape as the controllers runner).
const WORKERS: usize = 2;

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Claim pods whose `spec.schedulerName` is empty or equals this.
    pub scheduler_name: String,
    /// HTTP extenders (Q3 seam); empty = default plugins only.
    pub extenders: Vec<ExtenderConfig>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulerConfig {
    pub fn new() -> Self {
        Self {
            scheduler_name: DEFAULT_SCHEDULER_NAME.into(),
            extenders: Vec::new(),
        }
    }

    /// Load extender config from a `--scheduler-extender` JSON file:
    /// `{"extenders": [{url, filterVerb, prioritizeVerb, weight, ignorable,
    /// nodeCacheCapable, httpTimeoutMs}, ...]}` (a bare array is accepted).
    pub fn load_extenders(path: &std::path::Path) -> Result<Self, String> {
        let src =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let v: Value =
            serde_json::from_str(&src).map_err(|e| format!("parse {}: {e}", path.display()))?;
        let list = v
            .get("extenders")
            .and_then(|e| e.as_array())
            .cloned()
            .or_else(|| v.as_array().cloned())
            .ok_or_else(|| {
                format!(
                    "{}: expected {{\"extenders\": [...]}} or [...]",
                    path.display()
                )
            })?;
        let extenders: Vec<ExtenderConfig> = list
            .iter()
            .map(|e| serde_json::from_value(e.clone()))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("{}: invalid extender entry: {e}", path.display()))?;
        Ok(Self {
            scheduler_name: DEFAULT_SCHEDULER_NAME.into(),
            extenders,
        })
    }

    fn scheduler_name(&self) -> &str {
        if self.scheduler_name.is_empty() {
            DEFAULT_SCHEDULER_NAME
        } else {
            &self.scheduler_name
        }
    }
}

/// Spawner mirroring `controllers::ControllerManager::spawn`.
pub struct SchedulerManager;

impl SchedulerManager {
    /// Spawn the scheduler against the shared storage Arc. Returns the
    /// joinable task handles (leader elector + supervisor), stopped via
    /// `stop.trigger()`.
    pub fn spawn(
        store: Arc<dyn StorageBackend>,
        cfg: SchedulerConfig,
        stop: Stop,
    ) -> Vec<JoinHandle<()>> {
        let client: Arc<dyn Client> = Arc::new(StorageClient::new(store));
        let lease = LeaseConfig {
            namespace: "kube-system".into(),
            name: "init-pro-scheduler".into(),
            identity: format!("init-pro-scheduler-{}", rand_suffix(5)),
            lease_duration: 30,
            retry_period: 500,
            now: std::sync::Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        };
        let (leader_handle, leader_rx) =
            LeaderElector::new(client.clone(), lease).spawn(stop.clone());

        let supervisor = tokio::spawn(async move {
            let caches = Arc::new(Caches::new());
            let queue = Arc::new(WorkQueue::new());
            let extenders = match ExtenderSet::from_configs(&cfg.extenders) {
                Ok(set) => Arc::new(set),
                Err(e) => {
                    tracing::error!(target: "init-pro", error = %e,
                        "invalid extender config; scheduler runs default plugins only");
                    Arc::new(ExtenderSet::from_configs(&[]).expect("empty extender set"))
                }
            };
            let mut leader_rx = leader_rx;
            loop {
                if stop.is_triggered() {
                    return;
                }
                if !*leader_rx.borrow_and_update() {
                    if leader_rx.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
                tracing::info!(target: "init-pro",
                    "scheduler active (leader via Lease init-pro-scheduler, Q18)");
                let set = spawn_set(&client, &cfg, &caches, &queue, &extenders, stop.clone());
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
                                tracing::info!(target: "init-pro", "lost leadership; stopping scheduler set");
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

fn abort_set(set: &[JoinHandle<()>]) {
    for h in set {
        h.abort();
    }
}

/// Pending-pod keys from a cache snapshot (node fan-out + backstop resync).
fn enqueue_pending(pods: &ObjectStore, queue: &WorkQueue, scheduler_name: &str) {
    for p in pods.list(None) {
        if is_pending(&p, scheduler_name) {
            if let Some(name) = object::name(&p) {
                let ns = object::namespace(&p).unwrap_or("default");
                queue.add(format!("{ns}/{name}"));
            }
        }
    }
}

/// Informer caches shared across leadership terms (one per object kind).
pub struct Caches {
    pub pods: Arc<ObjectStore>,
    pub nodes: Arc<ObjectStore>,
    pub pvcs: Arc<ObjectStore>,
}

impl Default for Caches {
    fn default() -> Self {
        Self::new()
    }
}

impl Caches {
    pub fn new() -> Self {
        Caches {
            pods: Arc::new(ObjectStore::new()),
            nodes: Arc::new(ObjectStore::new()),
            pvcs: Arc::new(ObjectStore::new()),
        }
    }
}

/// Build the informers + workers + backstop for one leadership term.
fn spawn_set(
    client: &Arc<dyn Client>,
    cfg: &SchedulerConfig,
    caches: &Arc<Caches>,
    queue: &Arc<WorkQueue>,
    extenders: &Arc<ExtenderSet>,
    stop: Stop,
) -> Vec<JoinHandle<()>> {
    let Caches { pods, nodes, pvcs } = &**caches;
    let scheduler_name = cfg.scheduler_name().to_string();

    // Pod events -> enqueue when pending (ours to schedule).
    let q = queue.clone();
    let gate = scheduler_name.clone();
    let pod_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        let Some(obj) = event_object(ev) else { return };
        if is_pending(&obj, &gate) {
            if let Some(k) = event_object_key(ev) {
                q.add(k);
            }
        }
    });

    // Node events (Put only) -> retry every pending pod: capacity changed.
    let q = queue.clone();
    let pods_for_nodes = pods.clone();
    let gate = scheduler_name.clone();
    let node_handler: EventHandler = Arc::new(move |ev: &WatchEvent| {
        if !matches!(ev, WatchEvent::Put(_)) {
            return;
        }
        enqueue_pending(&pods_for_nodes, &q, &gate);
    });

    // PVC informer feeds the (v1 passthrough) VolumeBinding snapshot.
    let informers: Vec<(KeyPrefix, Arc<ObjectStore>, EventHandler)> = vec![
        (KeyPrefix::new("", "pods", None), pods.clone(), pod_handler),
        (
            KeyPrefix::new("", "nodes", None),
            nodes.clone(),
            node_handler,
        ),
        (
            KeyPrefix::new("", "persistentvolumeclaims", None),
            pvcs.clone(),
            Arc::new(|_ev: &WatchEvent| {}),
        ),
    ];
    let mut handles: Vec<JoinHandle<()>> = informers
        .into_iter()
        .map(|(prefix, store, handler)| {
            let client = client.clone();
            let stop = stop.clone();
            let inf = Informer::new(prefix);
            tokio::spawn(async move {
                inf.run(client, store, handler, stop).await;
            })
        })
        .collect();

    // Backstop resync (missed edges only; write-if-changed keeps it cheap).
    {
        let queue = queue.clone();
        let pods = pods.clone();
        let gate = scheduler_name.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(RESYNC);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tick.tick() => enqueue_pending(&pods, &queue, &gate),
                }
            }
        }));
    }

    // Workers: inline reconcile (the controllers-runner loop shape).
    for _ in 0..WORKERS {
        let client = client.clone();
        let pods = pods.clone();
        let nodes = nodes.clone();
        let pvcs = pvcs.clone();
        let queue = queue.clone();
        let extenders = extenders.clone();
        let stop = stop.clone();
        let gate = scheduler_name.clone();
        handles.push(tokio::spawn(async move {
            let filters: Vec<Box<dyn Filter>> = default_filters();
            let scores: Vec<Box<dyn Score>> = default_scores();
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    key = queue.next() => {
                        let Some(key) = key else { break };
                        reconcile_key(
                            &client, &gate, &pods, &nodes, &pvcs, &queue,
                            &filters, &scores, Some(extenders.as_ref()), &key,
                        ).await;
                    }
                }
            }
        }));
    }
    handles
}

/// One pod reconciliation: gate -> snapshot -> cycle -> bind/unschedulable.
#[allow(clippy::too_many_arguments)]
async fn reconcile_key(
    client: &Arc<dyn Client>,
    scheduler_name: &str,
    pods: &Arc<ObjectStore>,
    nodes: &Arc<ObjectStore>,
    pvcs: &Arc<ObjectStore>,
    queue: &Arc<WorkQueue>,
    filters: &[Box<dyn Filter>],
    scores: &[Box<dyn Score>],
    extenders: Option<&ExtenderSet>,
    key: &str,
) {
    let (ns, name) = match key.split_once('/') {
        Some((ns, name)) => (ns, name),
        None => ("default", key),
    };
    let result = match pods.get(ns, name) {
        None => Ok(()), // deleted from cache: nothing to do
        Some(pod) => {
            if !is_pending(&pod, scheduler_name) {
                queue.done(key);
                return; // bound / not ours: drop quietly (no requeue)
            }
            let snap = crate::plugin::Snapshot::build(
                &pods.list(None),
                &nodes.list(None),
                &pvcs.list(None),
            );
            match schedule_one(&pod, &snap, filters, scores, extenders).await {
                crate::Outcome {
                    node: Some(node), ..
                } => bind::bind_pod(client, &pod, &node).await,
                crate::Outcome { node: None, reason } => {
                    bind::mark_unschedulable(client, &pod, &reason)
                        .await
                        .map(|_| ())
                }
            }
        }
    };
    match result {
        Ok(()) => queue.done(key),
        Err(e) if e.is_conflict() || e.is_not_found() => {
            queue.done(key);
        }
        Err(e) => {
            tracing::warn!(target: "init-pro", pod = %key, error = %e,
                "scheduling attempt failed; rate-limited retry");
            queue.add_rate_limited(key.to_string());
            queue.done(key);
        }
    }
}
