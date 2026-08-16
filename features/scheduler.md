# scheduler (T3.2)

The kube-scheduler equivalent: a pure-function plugin framework (filter +
score) with default plugins and an HTTP extender seam, running in-process
on the controllers framework. Q23 is the ADR of record
(`plans/init-pro/decisions.md`); as-built detail in
`plans/init-pro/plan/03-control-plane.md`.

## Shape

- In-process with the server (Q19; no separate binary). Reuses the
  controllers informer/workqueue framework: pods/nodes/PVCs caches, ONE
  pending-pod workqueue, Lease+CAS leader election (`init-pro-scheduler`,
  Q18 pattern).
- Bind through the apiserver binding subresource
  (`crates/apiserver/src/binding.rs`): 201 / 404 / 409 already-bound /
  422 unknown node, upstream parity.

## Plugins (pure functions over an immutable `Snapshot`)

Filters (all must pass):
- NodeName, NodeUnschedulable (cordon + `spec.nodeName` pre-set),
- TaintToleration (all taint effects; missing `nodeTaint` tolerated),
- NodeAffinity — `spec.nodeSelector`, required `nodeAffinity`
  (OR-terms / AND-expressions), taint `nodeTaint` key,
- PodAntiAffinity — required topology segments (namespace + labels +
  topology key) and preferred penalties,
- ResourceFit — quantity math (decimal + binary SI, milli/micro/nano),
  init containers summed as the safe bound,
- VolumeBinding passthrough (PVC `spec.volumeName` / nodeAffinity).

Scores (higher wins):
- LeastRequested (avg cpu+memory free %), NodeAffinityPreferred,
  PodAntiAffinityPreferred. See `crates/scheduler/src/score.rs`.

## Q23 semantics worth remembering

- **Logical nodes** (no `status.allocatable`) are treated as unbounded
  and log-once — k3s/other schedulers may create nodes before the kubelet
  reports capacity. Real nodes honor allocatable minus assigned requests.
- **Anti-oscillation:** `pod.spec.schedulerName` defaults aside, an
  Unschedulable write is write-if-changed and a failed attempt requeues
  ONLY on pod/node events or a 30 s backstop — no hot loop
  (integration test asserts `resourceVersion` quiesces).
- Test gotcha: JSON-pointer lookups must fetch `/metadata/labels` as an
  object and `.get(key)` — pointer-escaping breaks on label keys
  containing `/` (e.g. `kubernetes.io/hostname`).

## Extender seam

Upstream KubeSchedulerConfiguration `extenders` wire shape, loaded from
`--kube-scheduler-arg config=<file>` (JSON; see `extender.rs`):
`urlPrefix` (alias `url`), `filterVerb`, `prioritizeVerb`, `weight`,
`ignorable`, `nodeCacheCapable`, `httpTimeoutMs`.
- HTTP-only in v1 (raw `TcpStream`, Q21 pattern; `https://` rejected),
- non-ignorable filter error fails the attempt (no partial bind);
  ignorable degrades to in-tree plugins,
- filter request lists nodes (+ pods when `nodeCacheCapable=false`);
  prioritize response `host/score` pairs are weighted into the final sum.

## Flags

- `--kube-scheduler-arg KEY=VALUE` (repeatable, real k3s flag). Only
  `config=<path>` is wired (extenders); other keys WARN no-op.
- `--disable-scheduler` skips spawning (and `--disable-controller-manager`
  is now honored too).

## Proof

- 26 unit + 5 integration tests in `crates/scheduler/` (extender stubs
  must mount BOTH `/filter` and `/prioritize` — a non-ignorable filter
  error aborts the attempt).
- Golden G22 (nodeSelector placement + Unschedulable settle) and G23
  (python3 stub extender steering, second server + `--kube-scheduler-arg`)
  → 23/23.
