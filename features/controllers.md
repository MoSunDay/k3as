# controllers

The kube-controller-manager-equivalent control loops (TODO **T3.1**, done
in T3.1a + T3.1b). Lives in `crates/controllers/`; spawned leader-gated
by `crates/cli/src/runtime.rs` in the apiserver branch.

## What it is

The client-go machinery every upstream controller is built on — informer,
workqueue, Lease+CAS leader election (**Q18**) — plus the reconcilers that
turn "API + storage" into a self-converging cluster: ReplicaSet,
Deployment (incl. rolling update), StatefulSet, DaemonSet, Endpoints,
garbage collector, and namespace lifecycle. The manager shares the
apiserver's storage `Arc` in process (**Q19**).

## As-built map (crates/controllers/src/)

| Module | Role |
|--------|------|
| `client.rs` | `Client` trait + `StorageClient` (in-process transport over `Arc<dyn StorageBackend>`, Q19); wire-identical `resourceVersion` projection. |
| `informer.rs` | LIST → cache → watch-from-max_revision; re-list on lag/close; synthetic initial events; event namespace normalized from the storage path; `ObjectStore` for sync-handler reads. |
| `workqueue.rs` | client-go semantics: dirty/processing dedup, rate-limited requeue (pure `backoff_for`), forget; **atomic done/next hand-off** (a concurrent `add` during `done()` can't strand a key). |
| `leaderelection.rs` | Q18: coordination.k8s.io Lease — acquire=create, renew=CAS, expired=CAS takeover; injected `NowFn` clock. |
| `object.rs` / `id.rs` / `time.rs` | JSON object semantics (selectors, ownership, `semantic_eq`, readiness); FNV template hash, label+suffix ids; RFC3339 without chrono. |
| `controllers/replicaset.rs` | Count owned pods; 5-char suffix names; delete surplus preferring unscheduled. |
| `controllers/deployment.rs` + `rollout.rs` + `conditions.rs` | Template-hash RS; `maxSurge`/`maxUnavailable` rolling pacing (raw JSON specs, upstream 25%/25% defaults); NewReplicaSetAvailable / ProgressDeadlineExceeded transitions. |
| `controllers/statefulset.rs` + `ordinal.rs` | Q22: `<sts>-<ordinal>` identity, OrderedReady/Parallel; PVC object per claim template per ordinal (never deleted on scale-down); ControllerRevision `<sts>-<hash10>`; RollingUpdate/OnDelete. |
| `controllers/daemonset.rs` | Node list as source of truth; nodeSelector + first nodeAffinity term; one pinned pod per matching node; status numbers track node lifecycle. |
| `controllers/endpoints.rs` | Service selector matching; ready = Ready True or no conditions. |
| `controllers/gc.rs` | Q20: managed-owner absence sweep (DELETE-driven + 2s backstop); annotation-marked Orphan. |
| `controllers/namespace.rs` | Q20: drain every namespaced kind, then terminal delete. |
| `runner.rs` | `ControllerManager::spawn` — leader-gated informers/workqueues/workers wiring. |

## Companion: `kubectl rollout status` (Q21)

`crates/kubectl/src/rollout.rs`: pure `evaluate` fn (observedGeneration,
deadline, complete, waiting messages — total, odd fields default) + a
250 ms poll loop. Exit 0 = rolled out, 1 = NotFound / deadline.

## Evidence

489 workspace tests; golden **21/21** — G17 (scale 1→3→1 + endpoints),
G18 (rolling update + rollout exit codes), G19 (ordinal identity + PVC
retention), G20 (per-node placement + node lifecycle), G21 (GC cascade +
namespace drain/finalize). SSOT: `plans/init-pro/index.md` (T3.1 done).
