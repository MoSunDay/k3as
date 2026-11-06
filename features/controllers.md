# controllers

The kube-controller-manager-equivalent control loops (TODO **T3.1**, done
in T3.1a + T3.1b). Lives in `crates/controllers/`; spawned leader-gated
by `crates/cli/src/runtime.rs` in the apiserver branch.

## What it is

The client-go machinery every upstream controller is built on — informer,
workqueue, Lease+CAS leader election (**Q18**) — plus the reconcilers that
turn "API + storage" into a self-converging cluster: ReplicaSet,
Deployment (incl. rolling update), StatefulSet, DaemonSet, Endpoints,
garbage collector, and namespace lifecycle. Post-T3.1 the same framework
also runs the Workflow/CronWorkflow controllers (T7.3, Q30), the node
podCIDR allocator (T4.3 Scope A, Q34), and schedules `init-pro.io/v1`
Agents (T7.1, Q35). The manager shares the
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
| `controllers/endpoints.rs` | Service selector matching; ready = Ready True or no conditions; `resolve_target_port` (absent→identity, numeric→verbatim, named→containerPort lookup, unresolvable→port omitted) + real kubelet podIPs preferred over the 10.42.x.y placeholder (Sprint 18, feeds the Q28 NodePort plane). |
| `controllers/gc.rs` | Q20: managed-owner absence sweep (DELETE-driven + 2s backstop); annotation-marked Orphan. |
| `controllers/namespace.rs` | Q20: drain every namespaced kind, then terminal delete. |
| `controllers/node_cidr.rs` | T4.3 Scope A (Q34): per-Node `spec.podCIDR` /24 allocation from `--cluster-cidr` (default 10.42.0.0/16). |
| `controllers/workflow*.rs` | T7.3 (Q30): Argo Workflow controller split into pure stages — `workflow_plan.rs` (steps entrypoint -> parallel groups), `workflow_dag.rs` (Kahn layering), `workflow_pod.rs` / `workflow_agent.rs` (step Pod vs `init-pro.io/v1` Agent child assembly, T7.1/Q35), `workflow_phase.rs` (naming + phase math), `workflow_status.rs` (node-tree status) — driven by `workflow.rs`. |
| `controllers/cronworkflow.rs` + `cron_expr.rs` | T7.3 Scope C (Q30): CronWorkflow fires child Workflows per `concurrencyPolicy`; `cron_expr.rs` is the pure cron parse + next-fire engine (robfig `@every` family). |
| `runner.rs` | `ControllerManager::spawn` — the leadership supervisor; informer fan-out wiring and the worker loop were extracted (per the runner.rs header) into the two modules below. |
| `runner_wiring.rs` / `runner_worker.rs` | A5 split out of `runner.rs` (code motion): per-resource informer -> workqueue -> worker wiring; worker drain + `reconcile_key` dispatch per kind. |
| `error.rs` / `stop.rs` | Controller error surface (CAS races -> `Conflict` = benign requeue); self-contained LATCHED stop token (deliberately not `infra::Shutdown`, Q19 decoupling). |

## Companion: `kubectl rollout status` (Q21)

`crates/kubectl/src/rollout.rs`: pure `evaluate` fn (observedGeneration,
deadline, complete, waiting messages — total, odd fields default) + a
250 ms poll loop. Exit 0 = rolled out, 1 = NotFound / deadline.

## Evidence

~937 workspace `#[test]`/`#[tokio::test]` annotations; golden inventory +
PASS-site count live in the SSOT `golden/README.md` (G01-G31, 39 PASS
sites) — G17 (scale 1→3→1 + endpoints),
G18 (rolling update + rollout exit codes), G19 (ordinal identity + PVC
retention), G20 (per-node placement + node lifecycle), G21 (GC cascade +
namespace drain/finalize). SSOT: `plans/init-pro/index.md` (T3.1 done).
