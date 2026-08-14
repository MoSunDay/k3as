# controllers

The kube-controller-manager-equivalent control loops (TODO **T3.1a**, first
slice of T3.1). Lives in `crates/controllers/`; spawned by
`crates/cli/src/runtime.rs` in the apiserver branch.

## What it is

The client-go machinery every upstream controller is built on — informer,
workqueue, leader election — plus the first three reconcilers that turn
"API + storage" into a self-converging cluster: ReplicaSet, Deployment,
Endpoints. The manager is leader-gated (only the Lease holder runs loops)
and shares the apiserver's storage `Arc` in process (decision **Q19**).

## As-built map (crates/controllers/src/)

| Module | Role |
|--------|------|
| `client.rs` | `Client` trait + `StorageClient` (in-process transport over `Arc<dyn StorageBackend>`, Q19); wire-identical `resourceVersion` projection. |
| `informer.rs` | LIST → cache → watch-from-max_revision; re-list on lag/close; synthetic initial events; `ObjectStore` for sync-handler reads. |
| `workqueue.rs` | client-go semantics: dirty/processing dedup, rate-limited requeue (pure `backoff_for`), forget. |
| `leaderelection.rs` | Q18 as-built: coordination.k8s.io Lease — acquire=create, renew=CAS, expired=CAS takeover (`leaseTransitions++`); injected `NowFn` clock for deterministic tests. |
| `object.rs` | JSON helpers: label selectors (matchLabels + matchExpressions), `controller_of`/ownerReference, `semantic_eq` write-if-changed guard, fnv template hash, rand suffix, placeholder pod IP. |
| `controllers/replicaset.rs` | Count owned pods by controller ownerRef; create with 5-char suffix names; delete surplus preferring unscheduled. |
| `controllers/deployment.rs` | Template-hash-named RS; instant scale-up + drain-down (Recreate-flavored v1); old RS deleted at `status.replicas==0`; status write-if-changed. |
| `controllers/endpoints.rs` | Service selector matching (LabelSelector + plain-map); ready = Ready True or no conditions; write-if-changed. |
| `runner.rs` | `ControllerManager::spawn` — leader-gated; 4 informers / 3 workqueues / 2 workers each; pod→owner-RS + matching-Service reverse enqueue; namespace bootstrap on becoming leader. |
| `stop.rs`, `time.rs`, `id.rs`, `error.rs` | Latched `Stop` token; RFC3339 without chrono; random ids; error type. |

## How acceptance is proven

- `crates/controllers/tests/controllers.rs` — 6 in-process e2e cases
  against the real `EmbeddedStorage` (no sockets): Deployment scale
  1→3→1 converges, pods converge, Endpoints reflect membership, and a
  quiesce-after-convergence anti-oscillation gate (no status churn once
  steady).
- Golden **G17** (`scripts/golden-conformance.sh`, 17/17): the real
  `init-pro server` binary — scale a Deployment 1→3→1, pods converge,
  Endpoints reflect membership. G16 pins the served apps/v1 group
  byte-stable.
- Plus `tests/informer.rs` (4) and `tests/leaderelection.rs` (4); 25 unit
  tests inline. Workspace total 399.

## Deliberately v1

- **Placeholder pod IPs** — deterministic 10.42.x.y hashed from the pod
  UID, until kubelet/CNI own addressing (T4.2/T4.3).
- **Ready-by-default** — a pod with NO conditions counts ready
  (documented default without kubelet; a Ready condition is honored when
  present).
- **Recreate-flavored rollout** — instant scale-up + drain-down; the
  `maxSurge` rolling strategy is T3.1b.
- **In-process transport (Q19)** — controllers share the apiserver's
  storage `Arc`; the HTTP-backed client swaps in at the `Client` trait
  boundary with T3.4 (HA) and T1.3 (auth).

## Status / next

- Landed: T3.1a (SSOT status `in-progress (T3.1a done)`).
- Remaining (T3.1b): StatefulSet, DaemonSet (schemas already served,
  inert), garbage collector, `kubectl rollout status` parity, `maxSurge`
  rolling update.

SSOT: `plans/init-pro/plan/03-control-plane.md` (T3.1) · decisions
**Q18** (Lease + CAS election) and **Q19** (in-process transport).
