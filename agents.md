# agents.md - init-pro agent entry point

Repository-local memory for init-pro. It points you at the SSOT and the
load-bearing conventions; it does not re-spec the system. Read this first,
then the feature cards in features/.

## Project at a glance

init-pro is a from-scratch Rust reimplementation of a k3s-compatible
Kubernetes distribution and the runtime for k3as, a multi-tenant Agent
sandbox platform. The supported deployment is one tenant per cluster with
tenant-dedicated nodes; Kubernetes Namespaces are not the tenant security
boundary. Agent action containment (Q63) and tenant-per-cluster isolation
are complementary boundaries. The distribution is shipped as ONE multicall
binary called `init-pro`.
`argv[0]` selects behavior: `server` | `agent` | `kubectl` | `ctr` |
`crictl` | `containerd` | `etcd` | `flannel` (symlink deployment just
works). It has a first-class built-in Router - an openrusty (h2c gateway,
Q32) programmable HTTP data plane with Ingress networking.k8s.io/v1 as
the first-class ingress resource - not a sidecar addon. Phase 1 (M0 + M1)
is complete; the Phase 2 execution plan is historical and Phase 3
(Agent sandbox + host-native AI) is active. The Cargo workspace lives under `crates/`
(21 crates).

## The SSOT (single source of truth)

- `plans/init-pro/index.md` - the TODO status table (54 TODOs across 9
  layers) + the critical-path DAG. Authoritative status of record.
- `plans/init-pro/plan/*.md` - per-layer detail mirroring the same TODO
  IDs (`00-foundation.md` ... `08-delivery.md`; Layer 4 is split
  `04-node.md` T4.1-T4.10 + `04b-builtin-engine.md` T4.11-T4.15; Layer 8
  is the delivery plane, `08-delivery.md` T8.1-T8.5); edit in lock-step
  with index.md.
- `plans/init-pro/decisions.md` - locked ADR-style decisions Q1-Q66
  (Context / Options / Decision / Consequences).
- `CHANGELOG.md` - detailed per-sprint retrospectives with auditable test
  counts.

NEVER contradict the SSOT. If you change semantics, update index.md + the
matching plan/*.md + decisions.md in lock-step.

SSOT drift watch (reconciled in Sprints 10-12; re-check on touch):
- T2.1/T2.2 status-vs-code drift was reconciled in Sprint 10 and T2.2 closed
  as done in Sprint 12 (watch replay + compaction landed; see CHANGELOG).
- `crates/router/src/lib.rs` previously claimed "T5.4 in progress, Scope A".
  NOW fixed: header says T5.4 done (hot reload implemented and tested); TLS
  is T5.5 and NOT started (post-Q32 there is no TLS code in the router).
- The repo's root project-instructions header (the `# agents.md` preamble fed to
  the agent) still describes the PRE-SPRINT-25 state (mlua/Lua Router,
  libsql/sqlite T2.3, T4.3 not started); trust index.md + CHANGELOG.md as
  the live source, not that preamble.
- OPEN (user-owned, do not edit on the agent's own initiative):
  `README.md:119-130` / `README_zh.md:119-125` still show the pre-Q32
  "Router VM (single mlua VM)" architecture diagram, and the CLI about
  string at `crates/cli/src/lib.rs:33` still says "built-in Lua Router" —
  post-Q32 the Router is a native Rust data plane on openrusty. Awaiting a
  user-driven README/CLI-about refresh; code and SSOT already tell the
  truth.

## Build & verify (canonical commands)

- COLD CLONES build deps first: `scripts/bootstrap-link-repos.sh`
  materializes the 6 pinned path-dep repos from `link_repos.lock` (Q38;
  T0.2b) — idempotent; verifies (never modifies) existing/symlinked dev
  checkouts. Run `cargo fmt --all` with link_repos ABSENT or you will
  reformat foreign pinned trees (CI orders fmt before bootstrap for this
  reason).
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
| `cli` | clap CLI + subcommands + flag-parity strip filter (T0.4); embedded flanneld runner + node-condition keeper (T4.3 Scope A, Q34); embedded workspace-NFS plane supervisor + workspace keeper (T7.4 Q36; S54 Q66: anyhome entity decommissioned — plane rides nfsserve + the in-tree DirFs, user-facing contract unchanged). |
| `infra` | tracing, layered config, graceful shutdown (T0.3). |
| `common` | Shared primitives: embed descriptor + `version()`; workspace path conventions — `MOUNT_TARGET=/workspace`, `/agent/{id}`, `/workflow/{id}` (T7.4, Q36). |
| `vendor` | Pinned upstream-artifact acquire + SHA-256 verify + SBOM (T0.2). |
| `api` | Resource model, schema registry, discovery builders, init-pro.io CRDs (T1.1). |
| `agent` | Pure seam: `AgentSpec` ↔ opencoder session config bridging (T7.1 Phase A, Q35); no execution in-crate (Phase B executes in cli/src/runtime.rs, done Sprint 30, Q37). |
| `apiserver` | HTTP discovery + REST CRUD/watch/SSA over the storage trait (T1.2 done). |
| `controllers` | kube-controller-manager-equivalent loops: informer/workqueue framework, Lease+CAS leader election, ReplicaSet/Deployment/Endpoints reconcilers + node podCIDR reconciler (T3.1; T4.3 Scope A, Q34). |
| `scheduler` | kube-scheduler-equivalent: filter/score plugin framework, default plugins, HTTP extender seam, in-process (T3.2, Q23). |
| `runtime` | Agent-side containerd: config templating, idempotent vendor staging (incl. CNI conflists + flannel aux staging/`aux/flannel` symlink), supervisor + CRI plumbing (T4.1, Q24-Q26; T4.3 Scope A). |
| `kubelet` | Kubelet-equivalent: watch+sync pod lifecycle over the CRI seam, node registration + Lease heartbeat, pods/status writer incl. podIP/podIPs/hostIP emission (T4.2 Scope A+B: probes/volumes/pull-policy/hostname, S20). |
| `kubectl` | kubectl-compatible client subset: HTTP transport + `rollout status` polling (T3.1b, Q21). |
| `router` | Built-in Router riding openrusty (h2c gateway, Q32): Ingress networking.k8s.io/v1 -> route compiler, balancer, reverse proxy, hot reload (T5.1-T5.4 done; TLS = T5.5 not started). |
| `storage` | `StorageBackend` trait + `EmbeddedStorage` + `RdbStorage` (RocksDB via the rdb crate; durable opt-in `rdb://`) incl. watch replay + compaction (T2.1/T2.2/T2.3, Q33). |
| `registry` | Embedded Docker Registry HTTP API v2 image plane, default-on in `server`: global blob CAS + per-repo manifests/tags, newest-N tag retention with keep-forever patterns excluded (`--registry-addr` / `--disable-registry` / `--registry-tag-retention` / `--registry-keep-tag`, T6.3, Q40); also hosts the V2 push client + `docker save` importer used by the delivery plane (T8.1). |
| `nsrun` | The only `unsafe` island: clone3 + six namespaces (NEWNS/NEWPID/NEWUTS/NEWIPC/NEWCGROUP + mount/pivot_root), child credential/cap setup, parent start gate + waitpid (T4.11, Q48). |
| `engine-proto` | Built-in engine frame protocol types: length-prefixed frames (`u32 LE len`, `u8 kind`, payload) + `ContainerSpec` (T4.11, Q48). |
| `engine` | `nsrun`-based single-threaded container engine process (`engine serve` / `engine-run`): one container per process, socket per container, pause-sandbox mode (T4.11-T4.13, Q48/Q49). |
| `engine-client` | Probe/CLI wire client for the engine frame protocol (boot probe used by the builtin-backend selection, T4.12, Q51). |

## Critical path & current state

- Critical path (platform): T0.1 -> T0.2 -> T2.1 -> T2.2 -> T1.2 ->
  T3.1/T3.2 -> T4.2 -> end-to-end cluster (M3).
- De-risk path (Q5): T0.1 -> T0.3 -> T5.1 -> T5.2 -> T5.4 (the M1 spike).
- T0.6 (Q59 supported-subset golden gate) is a merge gate, not a node: every
  TODO must keep `scripts/golden-conformance.sh` green.
- Done: Layer 0 (T0.1-T0.6 + T0.2b pin-clone bootstrap, Q38), T1.1/T1.2
  (api + apiserver), Layer 2 (T2.1/T2.2/T2.3 - storage, Q33), T3.1/T3.2
  (controllers + scheduler), T4.1 (containerd bundling), T4.2 Scope A+B+C
  (kubelet + logs/exec, Q31), the Layer 5 M1 slice (T5.1-T5.4, Q32),
  T4.6 (pod-log date dirs + rotation, Q39), T6.3 (embedded registry,
  Q40), T7.1 (`Agent` resource + node-side execution, Q35/Q37), T7.3
  (Argo Workflows, Q30; model/engine since extracted to the
  link_repos/arust-workflow reference, Q42), T7.4 (anyhome, Q36; entity
  decommissioned S54/Q66 — plane rebased on nfsserve + the in-tree DirFs,
  contract intact), and T7.5 (anyhome Pod-level annotation mounts, Q43).
  Phase 1 (M0 + M1) is complete; 43/54 TODOs done.
- Delivery plane (Layer 8, T8.1-T8.5 done in Sprints 45-46, Q56/Q57/Q58):
  certs.d plain-HTTP image trust (`hosts.toml`, T8.1) + registry V2 push
  client + `docker save` importer; `TrafficSplit` (`ts`) weighted one-click
  switching folded into the Router's NodePort plane (T8.2, Q57);
  `ServicePipeline`/`Build` embedded CI/CD (default-on, `--disable-pipeline`
  / `--pipeline-dir`, Q56) with full-object CAS status; an in-process
  pipeline e2e (real registry v2 + fake docker shim, T8.4); and the
  pipeline-owned canary release (T8.5, Q58) — `<deployment>-canary` clone +
  canary Service + stepped TrafficSplit weights with wall-clock soak, then
  promote (stable image patch, weights 100/0) or abort (`spec.release.abort`
  recycles the children). Layer 8 is closed out (M1-M3); other unfinished
  platform work includes T4.10 (kubelet volumes), T5.6 (SNI/TLS remainder,
  blocked on T5.5) and the T4.3 remainder (host-gw/multi-node, netpol = T4.4).
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
- Next gates on the active Phase 3 path (`plans/init-pro/phase-3-host-ai.md`):
  **P0 T7.7 (Q63)** adds Agent action interception and Linux sandboxing by
  extending done T7.1 and reusing done T4.14; it is independent of the
  inference track. The inference track remains **P1 T4.15 -> P2 T7.2 -> P3
  T7.6** (Q60-Q62): host-direct ordinary-inference performance, direct
  whole-GPU discovery/allocation, then llama.cpp OpenAI-compatible serving.
  T4.2 is closed (Scope A+B+C, Sprint 24), T4.3 Scope A is done (flannel CNI
  embedded in the agent, Q34 — F1-F5) and Scope B's ServiceLB (announce +
  raw-TCP L4, Q41) is done. The flannel host-gw/multi-node remainder, T5.6's
  SNI/TLS remainder (blocked on T5.5), and T4.4 (netpol) remain tracked
  separately from these Phase 3 branches.
- The server serves real REST CRUD/watch/SSA (kubectl-compatible); with
  T2.3, zero-restart durability arrived as an explicit opt-in
  (`rdb://` endpoint; `scripts/durability-e2e.sh` D1-D4 gates it),
  while the default embedded store stays in-memory.

## Decisions that matter (Q-codes)

- Q1 - single multicall binary (argv[0] dispatch; symlink deployment).
- Q2 - historical full k3s/k8s protocol conformance goal; Q59 narrows only
  the forward delivery scope (supported APIs remain wire-compatible).
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
- Q36 - anyhome is an embedded first-class NFS memory service (default-on,
  `--disable-anyhome`); workflow step pods get `/workspace` auto-mounted
  from `/workflow/<name>` and Agents carry the `init-pro.io/workspace`
  annotation; workspace path SSOT lives in `common` (D1: the binary path
  must not link `agent` — duplicate libsql symbols). S54/Q66: the any-home
  ENTITY (MemoryDb/libsql/embedder/anyhome.db) is decommissioned — the
  plane rides nfsserve + the in-tree DirFs (mirrorfs port, BSD-3-Clause),
  every user-facing surface unchanged; D1 is retained as link-graph
  hygiene (the vendored-libsql collision rationale is gone).

- Q37 - T7.1 Phase B: the `agent` subcommand's watch+execute loop runs a
  bound Agent as ONE opencoder CLI subprocess on its node (credentials
  only via the child env, Q35 D1) and merge-patches the status back.
- Q38 - `link_repos.lock` is the pin SSOT; `scripts/bootstrap-link-repos.sh`
  materializes the 7 path-dep repos before compiling (CI + Dockerfile).
  Release posture: intranet h2c on openrusty `4180ad3`; TLS later = pin
  bump + forward adaptation in `gateway_config.rs` (P2-1, queued).
- Q56 - T8.3: `ServicePipeline`/`Build` is the embedded CI/CD plane
  (default-on, `--disable-pipeline`) - one immutable `Build` child per tag
  transition, controller drives the host `docker` CLI and pushes through the
  embedded registry (Q40); status is full-object CAS (Q10).
- Q57 - T8.2: `TrafficSplit` (`ts`) is the weighted-switching primitive - the
  Router folds splits into per-endpoint openrusty weights (scaled by 100,
  deterministic remainder, zero-weight backends dropped), no service mesh.
- Q58 - T8.5 (LOCKED S46): pipeline-owned canary release - `spec.release`
  drives `<deployment>-canary` + a cloned canary Service + stepped
  TrafficSplit weights (wall-clock soak on the 2s pipeline resync), then
  promote (stable image patch + weights 100/0, children retained) or abort
  (`spec.release.abort`, idempotent, recycles the children); no health-gated
  auto-abort in M3.
- Q59 - forward delivery is a limited/supported Kubernetes protocol subset
  for ecosystem compatibility, superseding Q2's scope only; supported APIs
  stay upstream wire-compatible, Q10 JSON-only is unchanged, and T0.6 stays
  done as the supported-subset golden gate (no 100%/full-conformance claim).
- Q60 - T4.15 built-in host-direct is a performance profile for ordinary
  inference inside a tenant-dedicated cluster, not Agent action containment
  or a cross-tenant boundary. It uses host network/rootfs, clone3 necessary
  namespaces (NEWNS/NEWPID/NEWUTS/NEWIPC/NEWCGROUP, no NEWNET), literal
  `init-pro.io/rootfs: /`, no image pull/copy/pivot for `/`, host `/dev`
  access, and no containerd fallback (silent or explicit); nodes must not be
  shared across tenants. Agent workloads use Q63 instead. T4.11-T4.14/Q48-Q55 remain done.
- Q61 - T7.2 discovers and allocates direct host whole GPUs through standard
  extended resources for ordinary inference inside a tenant-dedicated cluster,
  builtin first; no device-plugin or fractional-GPU support in the first slice
  and no containerd fallback (silent or explicit) for GPU requests. Fake tests
  provide routine, fake-scope evidence only; real-hardware execution may be
  opt-in for ordinary CI, but recorded real evidence for every claimed GPU
  vendor is mandatory to close whole T7.2.
- Q62 - T7.6 serves pre-staged GGUF with llama.cpp through a standard
  Deployment + NodePort Service and an OpenAI-compatible API; no new CRD or
  Agent reuse. Fake tests provide routine, fake-scope evidence only;
  real-hardware execution may be opt-in for ordinary CI, but closing whole
  T7.6 requires recorded real evidence with pinned real llama-server, local
  read-only GGUF, and a real GPU.
- Q63 - T7.7 extends done T7.1/Q35/Q37 with a node/operator-owned default-deny
  action policy, a central tool/subagent gate, and Linux enforcement through
  existing nsrun/clone3/cgroups: private non-host rootfs and fresh network
  namespace, non-root/no_new_privs/empty capabilities, mandatory CPU/memory/
  PID limits, and descendant cleanup. A node-side allowlisted provider broker
  uses a per-attempt Unix socket and injects credentials upstream; sandboxed
  Agent/opencoder/tool/subagent environments receive none. Q63 supersedes
  child-env credential transport only for this sandboxed path, preserves the
  Q35/Q37 history, and forbids direct-process/host-root/host-network/
  containerd fallback when containment is unavailable.

- Q66 - S54: the anyhome ENTITY is decommissioned (any-home + libsql pins
  die; 6 pins remain); the workspace-NFS plane keeps every user-facing
  surface (flags/annotations/volume names/on-disk layout/keeper/Q43
  chain) and rides nfsserve directly with the in-tree DirFs (mirrorfs
  port, BSD-3-Clause) — "anyhome" is now the plane's historical name.

Read `plans/init-pro/decisions.md` for the full list (Q1-Q66).
