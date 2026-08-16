# Feature changelog (init-pro)

Feature-centric deltas, reverse chronological. NOT a duplicate of
`CHANGELOG.md` (which is sprint-centric with auditable test counts); this
ties changes to feature cards. Detailed per-sprint history: `CHANGELOG.md`.

- containerd-runtime: CREATED (T4.1 → done, Sprint 16, 2026-08-15) — `crates/runtime` (TOML v2 config templating w/ `INIT_PRO_SANDBOX_IMAGE`, idempotent SHA-256 staging + CNI loopback conflist, supervisor w/ socket-health gate, `base << restarts` backoff cap 5 s, STABLE_AFTER 30 s reset, SIGKILL+10 s drain, no SIGTERM per Q25); sticky `infra::Shutdown` (lost-wakeup fix, all consumers); agent CLI runtime-first drain; `init-pro crictl`/`ctr` pre-clap passthrough w/ endpoint injection; crictl v1.31.1 vendored. CRI client = crictl subprocess now, native gRPC behind explicit T4.2 trigger (Q26). +17 unit +2 integration (runtime), +5 multicall, +1 infra → 551 workspace green; golden G24 → 24/24 (sandbox-pull smoke SKIPs, registry-gated).
- scheduler: CREATED (T3.2 → done) — filter/score plugin framework on the controllers informer/workqueue + Lease election (Q23); 7 default filters (NodeName, cordon, TaintToleration, NodeAffinity incl. nodeSelector, PodAntiAffinity, ResourceFit with SI quantity math, VolumeBinding) + 3 scores; HTTP extender seam (upstream extenders wire shape, ignorable semantics); binding subresource in apiserver (201/404/409/422 parity); `--kube-scheduler-arg config=` wired + `--disable-scheduler`/`--disable-controller-manager` honored. Logical-node unbounded + log-once; Unschedulable write-if-changed with event/30 s requeue only. +26 unit +5 integration +4 binding tests → 526 workspace green; golden G22/G23 → 23/23.
- controllers: UPDATED (T3.1b, T3.1 → done) — Deployment rolling update (maxSurge/maxUnavailable + NewReplicaSetAvailable transitions), `kubectl rollout status` (Q21: pure evaluate + 250 ms poll, exit-code parity), StatefulSet (Q22: ordinal identity, per-ordinal PVC objects + ControllerRevisions), DaemonSet (Node-driven placement + status numbers), GC + namespace drain/finalize (Q20). Two bugs fixed: workqueue done/next hand-off race (keys could strand permanently) and apiserver PUT dropping `metadata.namespace` (desynced informer caches into no-op reconciles). +90 tests → 489; golden G18-G21 → 21/21.
- controllers: CREATED (T3.1a) — informer/workqueue framework + Lease+CAS leader election (Q18) + ReplicaSet/Deployment/Endpoints reconcilers, in-process `Client` transport over the shared storage `Arc` (Q19). 14 integration tests (incl. scale 1→3→1 convergence + quiesce gate); golden G16/G17 → 17/17. T3.1 → in-progress (T3.1a done); StatefulSet/DaemonSet/GC/rollout-status remain T3.1b.
- server-side-apply: CREATED (T1.2c) — SSA field-manager over the REST face. `crates/api/src/apply/` (field extraction by merge-key + merge/conflict/prune algorithm + managedFields round-trip) + `crates/apiserver/src/apply.rs` (PUT/PATCH dispatch on `application/apply-patch+yaml`, 201 create / 200 update / 409 conflict / `force` ownership steal). +15 tests (8 api + 7 apiserver); golden G13/G14 → 14/14. T1.2c deferred → done; T1.2 → done.
- storage-layer: CREATED. T2.1/T2.2 - `StorageBackend` trait +
  `EmbeddedStorage` (etcd-faithful revisions + CAS + broadcast watch) + 15
  integration tests. SSOT status table not yet updated to "done".
- router-data-plane: reached the M1 data plane (T5.4) - Ingress->route
  compiler, round-robin balancer, Rust reverse proxy, TLS termination /
  SNI, hot-reload seam. Completes Phase 1 (M0 + M1). `resty::*` + shared
  dicts (T5.3) and the phase pipeline + cosocket (T5.1/T5.2) preceded it.
- discovery-api: byte-correct over HTTP (T1.2a) on top of the T1.1
  resource model; golden conformance gate (T0.6) is 6/6 green.
- build-bundling: landed (T0.2) - pinned-artifact acquire + SHA-256 gate +
  zstd embed codegen + SPDX-2.3 SBOM + runtime `stage()`.
- multicall-cli: done (T0.1 + T0.4) - argv[0] dispatch + reexec peer
  stubs + k3s flag parity (wired / no-op / conflict rules).
