# agents.md - init-pro agent entry point

Repository-local memory for init-pro. It points you at the SSOT and the
load-bearing conventions; it does not re-spec the system. Read this first,
then the feature cards in features/.

## Project at a glance

init-pro is a from-scratch Rust reimplementation of a k3s-compatible
Kubernetes distribution, shipped as ONE multicall binary called `init-pro`.
`argv[0]` selects behavior: `server` | `agent` | `kubectl` | `ctr` |
`crictl` | `containerd` | `etcd` | `flannel` (symlink deployment just
works). It has a first-class built-in Router - an openrusty (h2c gateway,
Q32) programmable HTTP data plane with Ingress networking.k8s.io/v1 as
the first-class ingress resource - not a sidecar addon. Phase 1 (M0 + M1)
is complete; Phase 2 (persistence + control plane) is in progress. The Cargo workspace lives under `crates/` (15 crates).

## The SSOT (single source of truth)

- `plans/init-pro/index.md` - the TODO status table (33 TODOs across 8
  layers) + the critical-path DAG. Authoritative status of record.
- `plans/init-pro/plan/*.md` - per-layer detail mirroring the same TODO
  IDs (`00-foundation.md` ... `07-agent-scheduling.md`); edit in lock-step
  with index.md.
- `plans/init-pro/decisions.md` - locked ADR-style decisions Q1-Q35
  (Context / Options / Decision / Consequences).
- `CHANGELOG.md` - detailed per-sprint retrospectives with auditable test
  counts.

NEVER contradict the SSOT. If you change semantics, update index.md + the
matching plan/*.md + decisions.md in lock-step.

SSOT drift watch (reconciled in Sprints 10-12; re-check on touch):
- T2.1/T2.2 status-vs-code drift was reconciled in Sprint 10 and T2.2 closed
  as done in Sprint 12 (watch replay + compaction landed; see CHANGELOG).
- `crates/router/src/lib.rs` previously claimed "T5.4 in progress, Scope A".
  NOW fixed: header says T5.4 done (TLS + hot reload are implemented and tested).
- The repo's root project-instructions header (the `# agents.md` preamble fed to
  the agent) still describes the PRE-SPRINT-25 state (mlua/Lua Router,
  libsql/sqlite T2.3, T4.3 not started); trust index.md + CHANGELOG.md as
  the live source, not that preamble.

## Build & verify (canonical commands)

- `cargo build --workspace --locked` -> single binary at
  `target/debug/init-pro`.
- `cargo test --workspace --locked` -> all unit + integration tests.
- `cargo clippy --workspace --all-targets -- -D warnings` -> must be zero.
- e2e scripts under `scripts/` run a PRE-BUILT binary: build first, then
  e.g. `scripts/multicall-selftest.sh`. Other gates:
  `scripts/golden-conformance.sh`, `scripts/cli-flag-parity-test.sh`,
  `scripts/service-traffic-e2e.sh` (Service NodePort traffic, ST1-ST6;
  SKIPs itself without a vendor bundle/cc),
  `scripts/flannel-e2e.sh` (flannel CNI, F1-F5).
- `--locked` is mandatory for CI/release; dependencies are caret-only.
- Toolchain: stable (`rust-toolchain.toml`), MSRV 1.89, edition 2021.
- `crates/init-pro/build.rs` defaults to Auto acquire (no network) + empty
  embed registry so the dev loop and `cargo test` stay fast. Set
  `INIT_PRO_VENDOR=1` to download, `INIT_PRO_OFFLINE=1` to forbid network,
  `INIT_PRO_EMBED=1` to bake blobs into the binary.

## Hard conventions (enforced)

- `#![forbid(unsafe_code)]` in every lib crate.
- Pure-functional style: no classes/OOP inheritance; `struct` + `impl` +
  traits is fine (the established Rust idiom here).
- File size limits: new files <= 400 lines, iterating files <= 800 lines;
  split by responsibility if larger.
- Module layout: flat `src/*.rs` declared with `pub mod` in lib.rs;
  sub-directories use `mod.rs`; `pub use` re-exports at the crate root so
  consumers depend on the crate name only.
- JSON-only wire format for v1 (decision Q10); no protobuf - serde JSON
  is the sole transport for apiserver, storage, and watch streams.
- No hardcoded secrets/API keys/tokens; no destructive DB ops without
  explicit permission.
- Comment style: module-level `//!` doc comments reference task IDs
  (T1.1, T5.4) and decision codes (Q10, Q13).

## Workspace map

| Crate | Role |
|-------|------|
| `init-pro` | Single binary; argv[0] dispatch + build.rs asset bundling. |
| `multicall` | Alias table + reexec + peer stubs (T0.1). |
| `cli` | clap CLI + subcommands + flag-parity strip filter (T0.4); embedded flanneld runner + node-condition keeper (T4.3 Scope A, Q34). |
| `infra` | tracing, layered config, graceful shutdown (T0.3). |
| `common` | Shared primitives: embed descriptor + `version()`. |
| `vendor` | Pinned upstream-artifact acquire + SHA-256 verify + SBOM (T0.2). |
| `api` | Resource model, schema registry, discovery builders, init-pro.io CRDs (T1.1). |
| `agent` | Pure seam: `AgentSpec` ↔ opencoder session config bridging (T7.1 Phase A, Q35); no execution (Phase B). |
| `apiserver` | HTTP discovery + REST CRUD/watch/SSA over the storage trait (T1.2 done). |
| `controllers` | kube-controller-manager-equivalent loops: informer/workqueue framework, Lease+CAS leader election, ReplicaSet/Deployment/Endpoints reconcilers + node podCIDR reconciler (T3.1; T4.3 Scope A, Q34). |
| `scheduler` | kube-scheduler-equivalent: filter/score plugin framework, default plugins, HTTP extender seam, in-process (T3.2, Q19). |
| `runtime` | Agent-side containerd: config templating, idempotent vendor staging (incl. CNI conflists + flannel aux staging/`aux/flannel` symlink), supervisor + CRI plumbing (T4.1, Q24-Q26; T4.3 Scope A). |
| `kubelet` | Kubelet-equivalent: watch+sync pod lifecycle over the CRI seam, node registration + Lease heartbeat, pods/status writer incl. podIP/podIPs/hostIP emission (T4.2 Scope A+B: probes/volumes/pull-policy/hostname, S20). |
| `kubectl` | kubectl-compatible client subset: HTTP transport + `rollout status` polling (T3.1b, Q21). |
| `router` | Built-in Router riding openrusty (h2c gateway, Q32): Ingress networking.k8s.io/v1 -> route compiler, balancer, reverse proxy, TLS (T5.1-T5.6 Scope A done). |
| `storage` | `StorageBackend` trait + `EmbeddedStorage` + `RdbStorage` (RocksDB via the rdb crate; durable opt-in `rdb://`) incl. watch replay + compaction (T2.1/T2.2/T2.3, Q33). |

## Critical path & current state

- Critical path (platform): T0.1 -> T0.2 -> T2.1 -> T2.2 -> T1.2 ->
  T3.1/T3.2 -> T4.2 -> end-to-end cluster (M3).
- De-risk path (Q5): T0.1 -> T0.3 -> T5.1 -> T5.2 -> T5.4 (the M1 spike).
- T0.6 (golden conformance) is a merge gate, not a node: every TODO must
  keep `scripts/golden-conformance.sh` green.
- Done: Layer 0 (T0.1-T0.6), T1.1/T1.2 (api + apiserver), Layer 2
  (T2.1/T2.2/T2.3 - storage, Q33), T3.1/T3.2 (controllers + scheduler),
  T4.1 (containerd bundling), T4.2 Scope A+B+C (kubelet + logs/exec, Q31),
  the Layer 5 M1 slice (T5.1-T5.4, Q32), and T7.3 (Argo Workflows, Q30).
  Phase 1 (M0 + M1) is complete; 21/33 TODOs done (T7.1 Phase A, Q35).
- Storage is closed out: T2.1/T2.2 done (trait + `EmbeddedStorage` with
  etcd revision/CAS semantics, watch historical replay + compaction);
  T2.3 done since Sprint 25 on **rdb/RocksDB** — `RdbStorage`
  (rdb crate `store` feature, kine-style append-only `log/` semantics,
  fsync-ed WriteBatch) delivers zero-restart durability as an opt-in
  `--datastore-endpoint rdb://<path>` backend; the default stays
  `EmbeddedStorage` (Q17/Q33 — the Sprint-19 libsql/SQLite backend was
  removed and `sqlite://` now answers `Unsupported`). Leader election is
  Lease-object + CAS, not etcd leases (Q18). The real etcd-gRPC client is
  re-scoped to T3.4 (HA multi-server, its only consumer).
- Next gate on the critical path: T4.2 is closed (Scope A+B+C, Sprint 24)
  and T4.3 Scope A is done (flannel CNI embedded in the agent, Q34 —
  F1-F5); remaining on the path: T4.3 Scope B (ServiceLB L4 w/ T5.6,
  host-gw/multi-node/netpol) and T4.4 (netpol).
- The server serves real REST CRUD/watch/SSA (kubectl-compatible); with
  T2.3, zero-restart durability arrived as an explicit opt-in
  (`rdb://` endpoint; `scripts/durability-e2e.sh` D1-D4 gates it),
  while the default embedded store stays in-memory.

## Decisions that matter (Q-codes)

- Q1 - single multicall binary (argv[0] dispatch; symlink deployment).
- Q2 - full k3s/k8s protocol conformance (do not reinvent the API).
- Q4 - built-in Router as a first-class platform primitive (realized via
  openrusty as of Q32), not an addon.
- Q5 - M1 vertical slice de-risks the Router first (Ingress->real
  traffic; the original Lua spike is historical).
- Q10 - JSON-only wire format for v1 (no protobuf).
- Q12/Q13/Q14 - Router VM model, coroutine<->async bridge, per-phase
  coroutines and filter semantics (pre-Q32 Lua era; historical).
- Q32 - the Router rides openrusty (h2c gateway) as an embedded library;
  the mlua/Lua plane is deleted; Ingress networking.k8s.io/v1 is the
  first-class ingress resource.
- Q33 - the durable datastore option is `rdb://` (RocksDB via the rdb
  crate); the Sprint-19 libsql/SQLite backend is removed (`sqlite://`
  answers Unsupported).
- Q34 - CNI is flannel-rust embedded in the agent process (vxlan default)
  + the `flannel` multicall alias as CNI meta-plugin; node podCIDRs from
  `--cluster-cidr` via a controller reconciler.
- Q35 - `Agent` is an init-pro.io/v1 first-class resource scheduled natively
  through the same loop as Pods; Workflow `template.agent` creates Agents
  (no Pod materialization); opencoder embedded as link_repos path deps.

Read `plans/init-pro/decisions.md` for the full list (Q1-Q35).
