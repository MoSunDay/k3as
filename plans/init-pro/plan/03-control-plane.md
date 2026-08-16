# Layer 3 — Control Plane

Mirrors `index.md` TODO IDs **T3.1–T3.4**. The controllers, scheduler,
bootstrap/HA machinery that turn "API + storage" into a working cluster.

---

## T3.1 — kube-controller-manager 等价核心循环

- **目标 / Goal**
  The core control loops upstream ships in `kube-controller-manager`:
  `replicaset`, `deployment`, `statefulset`, `daemonset`, `endpoint`,
  `service`, `garbagecollector`, `namespace` lifecycle (k3s-relevant subset).

- **核心实现 / Core implementation**
  - `init-pro-controllers` crate; generic informer + workqueue pattern
    (kube-rs `Api::watch` + `Controller`).
  - Per-controller reconcile with upstream-identical status fields.
  - Leader election via `coordination.k8s.io` Lease + resourceVersion
    CAS (Q18) so only one server runs them — no etcd-lease dependency.
  - **As-built (T3.1a):** the client is a small `Client` trait with an
    in-process `StorageClient` over the `Arc<dyn StorageBackend>` shared
    with the apiserver (**Q19**); the HTTP-backed impl arrives with
    T3.4/T1.3. Informer = LIST → cache → watch-from-max_revision with
    re-list on lag/close; workqueue has client-go dirty/processing dedup
    + rate-limited requeue (the done/next hand-off is atomic — a
    concurrent `add` during `done()` can no longer strand a key).
    Placeholder deterministic 10.42.x.y pod IPs from UID hash and
    ready-by-default-without-conditions are the documented v1 defaults
    until kubelet/CNI (T4.2/T4.3). Lease election tests use an injected
    `NowFn` clock for deterministic expiry.
  - **As-built (T3.1b):** Deployment rolling update (`maxSurge`/
    `maxUnavailable` off raw JSON specs, upstream 25%/25% defaults;
    NewReplicaSetAvailable / ProgressDeadlineExceeded transitions;
    `controllers/rollout.rs` + `conditions.rs`); `kubectl rollout
    status` (**Q21**: pure `evaluate` + 250 ms poll loop, exit 0 rolled
    out / 1 NotFound-or-deadline); StatefulSet (**Q22**: `<sts>-<ordinal>`
    pods in order, OrderedReady gated on prior-ordinal readiness,
    Parallel; one PVC object per claim template per ordinal, never
    deleted on scale-down; every distinct template recorded as an
    `apps/v1` ControllerRevision `<sts>-<hash10>`; RollingUpdate +
    OnDelete); DaemonSet (Node list as source of truth; nodeSelector +
    first nodeAffinity matchExpressions term; terminating nodes never
    match; desired/current/ready/updated numbers); GC + namespace
    lifecycle (**Q20**: managed-owner absence sweep on DELETE events +
    2 s backstop, annotation-marked Orphan, namespace drain then
    terminal delete); apiserver PUT now defaults/validates
    `metadata.namespace` and the informer normalizes event namespaces
    from the storage path (namespace-less watch events had desynced
    caches into silent no-op reconciles).

- **验收手段 / Acceptance**
  - Golden (T0.6): G17 scale 1→3→1 + endpoints; G18 rolling update +
    `kubectl rollout status` exit codes; G19 StatefulSet ordinal
    identity + PVC retention; G20 DaemonSet per-node placement + node
    lifecycle; G21 GC cascade + namespace drain/finalize.

- **状态 / Status** — done (T3.1a Sprint 13, T3.1b Sprint 14)
- **证据 / Evidence** — Sprint 13 (T3.1a): `crates/controllers` —
  ReplicaSet/Deployment/Endpoints reconcilers over the informer/
  workqueue framework; 399 workspace tests green (+39). Sprint 14
  (T3.1b): rollout/conditions, StatefulSet (`statefulset.rs`,
  `ordinal.rs`), DaemonSet, GC (`gc.rs`), namespace (`namespace.rs`),
  `kubectl rollout status` (`crates/kubectl/src/rollout.rs`); 489
  workspace tests green (+90 across S1-S5, incl. a workqueue
  concurrent-done regression test); `scripts/golden-conformance.sh`
  21/21 — G18 (rolling update + rollout status exit codes), G19
  (ordinal identity + PVC retention), G20 (per-node placement + node
  lifecycle), G21 (GC cascade + namespace drain/finalize) on the real
  binary. clippy `-D warnings` + fmt clean.
- **卡点 / Blockers** — none for v1 scope; PV binding for StatefulSet
  PVCs arrives with T6.2 (**Q22**, Pending until then).
- **依赖 / Depends on** — T1.2, T2.2

---

## T3.2 — 调度器 (kube-scheduler 等价 + extender seam)

- **目标 / Goal**
  A scheduler assigning pods to nodes with the standard
  filter/score plugin model, **and a documented extender seam** so Layer 7
  (Q3) can plug in AI-agent policies later.

- **核心实现 / Core implementation**
  - `init-pro-scheduler`; plugin framework mirroring upstream scheduling
    framework (Filter/Score/Bind phases).
  - Default plugins: `NodeName`, `TaintToleration`, `NodeAffinity`,
    `PodAntiAffinity`, `ResourceFit`, `VolumeBinding`.
  - Extender seam: out-of-process HTTP scheduler extender
    (upstream-compatible) — Layer 7 hooks here.
  - Bind via API (T1.2); leader-elected single active scheduler.
  - **As-built (Sprint 15, Q23):** `crates/scheduler` — pure-function
    plugin layer (`Filter`/`Score` traits over an immutable `Snapshot`),
    7 default filters (NodeName, NodeUnschedulable, TaintToleration,
    NodeAffinity incl. `spec.nodeSelector`, PodAntiAffinity, ResourceFit,
    VolumeBinding passthrough honoring PVC `spec.nodeAffinity`), 3 default
    scores (LeastRequested, NodeAffinityPreferred,
    PodAntiAffinityPreferred); quantity math (decimal + binary SI,
    milli/micro/nano). Runner reuses the controllers crate (**Q23**):
    pods/nodes/PVCs informers + one pending-pod workqueue + Lease+CAS
    election `init-pro-scheduler` (Q18) over the in-process client (Q19).
    **Logical nodes** (no `status.allocatable`) are unbounded, logged
    once. Unschedulable is write-if-changed; requeue only on pod/node
    events or a 30 s backstop (anti-oscillation, revision-quiesce test).
    **Extenders are HTTP-only** (`https://` rejected in v1), upstream
    field names (`urlPrefix`, `filterVerb`, `prioritizeVerb`, `weight`,
    `ignorable`, `nodeCacheCapable`), ignorable-degrade vs
    fail-the-attempt semantics; wired via the real k3s flag
    `--kube-scheduler-arg config=<KubeSchedulerConfiguration.json>`.
    The apiserver serves `pods/{name}/binding` (201/404/409/422); the
    scheduler binds in-process (write-if-changed `spec.nodeName` +
    `PodScheduled=True`). Inter-pod affinity, Permit/PreBind, profile
    config: documented out of v1.

- **验收手段 / Acceptance**
  - Golden: a pod with `nodeSelector` lands only on matching nodes (G22:
    placement + PodScheduled=True + Unschedulable settle — 23/23).
  - Extender test: a stub extender rejects/accepts a pod deterministically
    (G23: python3 stub via a second server + `--kube-scheduler-arg`;
    in-process axum-stub integration tests cover filter-reject-all and
    prioritize-steer).

- **状态 / Status** — done
- **证据 / Evidence** — `crates/scheduler` (26 unit + 5 integration
  tests), `crates/apiserver/src/binding.rs` + tests (4),
  `scripts/golden-conformance.sh` G22/G23, `--kube-scheduler-arg` wired
  (snapshot + parity 16/16); 526 workspace tests green.
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T1.2, T4.1 (runtime dependency waived by the
  Q24 spike: scheduling operates on Node objects, not on containerd)

---

## T3.3 — bootstrap / 证书 / token 轮转

- **目标 / Goal**
  Cluster bootstrap: CA generation, serving/client cert issuance, node
  bootstrap tokens, and cert rotation (k3s `--cluster-init` / join flow).

- **核心实现 / Core implementation**
  - Self-signed CA on `--cluster-init`; join tokens (k3s
    `clientaccess.Token` parity).
  - `cfssl`/`rcgen` cert signing; kubelet client cert rotation via CSR
    auto-approval (a controller in T3.1).
  - Service-account token secrets + bound-token (v1) support.

- **验收手段 / Acceptance**
  - Golden: bring up server, join an agent via token, observe kubelet cert
    issued + rotated.
  - `openssl s_client` against API shows expected SANs/chain.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T1.3, T2.2

---

## T3.4 — HA 多 server (etcd 集群 + 选举)

- **目标 / Goal**
  Multi-server HA: embedded etcd clusters across server nodes, elected
  active controllers/scheduler, k3s `--cluster-init`/`--server` join flow.

- **核心实现 / Core implementation**
  - etcd cluster bootstrap + membership (T2.1) across N servers.
  - Per-component leader election via Lease objects + CAS (Q18; etcd
    TTL leases are an implementation detail of the real-etcd backend,
    not the election contract): only one apiserver-etcd-
    primary set of controllers runs.
  - Proxy/load-balancing of agent→server traffic (k3s agent LB,
    `link_repos/k3s/pkg/agent/loadbalancer/`).

- **验收手段 / Acceptance**
  - Golden: 3-server cluster survives killing the leader; controllers
    continue; no split-brain.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T2.2, T3.3
