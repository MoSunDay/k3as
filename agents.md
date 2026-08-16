# agents.md - init-pro agent entry point

Repository-local memory for init-pro. It points you at the SSOT and the
load-bearing conventions; it does not re-spec the system. Read this first,
then the feature cards in features/.

## Project at a glance

init-pro is a from-scratch Rust reimplementation of a k3s-compatible
Kubernetes distribution, shipped as ONE multicall binary called `init-pro`.
`argv[0]` selects behavior: `server` | `agent` | `kubectl` | `ctr` |
`crictl` | `containerd` | `etcd` (symlink deployment just works). It has a
first-class built-in Lua Router - an openresty-style programmable HTTP
data plane driven by Lua via mlua - not a sidecar addon. Phase 1 (M0 + M1)
is complete; Phase 2 (persistence + control plane) is in progress. The Cargo workspace lives under `crates/` (14 crates).

## The SSOT (single source of truth)

- `plans/init-pro/index.md` - the TODO status table (33 TODOs across 8
  layers) + the critical-path DAG. Authoritative status of record.
- `plans/init-pro/plan/*.md` - per-layer detail mirroring the same TODO
  IDs (`00-foundation.md` ... `07-agent-scheduling.md`); edit in lock-step
  with index.md.
- `plans/init-pro/decisions.md` - locked ADR-style decisions Q1-Q19
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
  the agent) still describes an OLDER state ("T5.4 in progress", "T2.1/T2.2 not
  started", "server discovery-only, zero persistence"); trust index.md +
  CHANGELOG.md as the live source, not that preamble.

## Build & verify (canonical commands)

- `cargo build --workspace --locked` -> single binary at
  `target/debug/init-pro`.
- `cargo test --workspace --locked` -> all unit + integration tests.
- `cargo clippy --workspace --all-targets -- -D warnings` -> must be zero.
- e2e scripts under `scripts/` run a PRE-BUILT binary: build first, then
  e.g. `scripts/multicall-selftest.sh`. Other gates:
  `scripts/golden-conformance.sh`, `scripts/cli-flag-parity-test.sh`.
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
| `cli` | clap CLI + subcommands + flag-parity strip filter (T0.4). |
| `infra` | tracing, layered config, graceful shutdown (T0.3). |
| `common` | Shared primitives: embed descriptor + `version()`. |
| `vendor` | Pinned upstream-artifact acquire + SHA-256 verify + SBOM (T0.2). |
| `api` | Resource model, schema registry, discovery builders, init-pro.io CRDs (T1.1). |
| `apiserver` | HTTP discovery + REST CRUD/watch/SSA over the storage trait (T1.2 done). |
| `controllers` | kube-controller-manager-equivalent loops: informer/workqueue framework, Lease+CAS leader election, ReplicaSet/Deployment/Endpoints reconcilers (T3.1). |
| `scheduler` | kube-scheduler-equivalent: filter/score plugin framework, default plugins, HTTP extender seam, in-process (T3.2, Q19). |
| `runtime` | Agent-side containerd: config templating, idempotent vendor staging, supervisor + CRI plumbing (T4.1, Q24-Q26). |
| `kubectl` | kubectl-compatible client subset: HTTP transport + `rollout status` polling (T3.1b, Q21). |
| `router` | Built-in Lua data plane: phase pipeline, resty.*, Ingress->route compiler, balancer, reverse proxy, TLS (T5.1-T5.4). |
| `storage` | `StorageBackend` trait + embedded etcd-semantics store incl. watch replay + compaction (T2.1/T2.2 done). |

## Critical path & current state

- Critical path (platform): T0.1 -> T0.2 -> T2.1 -> T2.2 -> T1.2 ->
  T3.1/T3.2 -> T4.2 -> end-to-end cluster (M3).
- De-risk path (Q5): T0.1 -> T0.3 -> T5.1 -> T5.2 -> T5.4 (the M1 spike).
- T0.6 (golden conformance) is a merge gate, not a node: every TODO must
  keep `scripts/golden-conformance.sh` green.
- Done: Layer 0 (T0.1-T0.6), T1.1/T1.2 (api + apiserver), T2.1/T2.2
  (storage), T3.1/T3.2 (controllers + scheduler), T4.1 (containerd
  bundling), and the Layer 5 M1 slice (T5.1-T5.4). Phase 1 (M0 + M1)
  is complete; 17/33 TODOs done.
- Storage is closed out: T2.1/T2.2 done (trait + `EmbeddedStorage` with
  etcd revision/CAS semantics, watch historical replay + compaction);
  durability + alternative backends are T2.3 (Q17). Leader election is
  Lease-object + CAS, not etcd leases (Q18).
- Next gate on the critical path: T4.2 (kubelet equivalent — pod
  lifecycle + status reporting) is the last big node before M3
  end-to-end; it builds on T4.1's CRI seam (Q26: vendored-crictl
  subprocess now, native gRPC later). T2.3 (durability) is the other
  open Layer 2 item.
- The server serves real REST CRUD/watch/SSA over the embedded store
  (kubectl-compatible); zero-restart durability arrives with T2.3.

## Decisions that matter (Q-codes)

- Q1 - single multicall binary (argv[0] dispatch; symlink deployment).
- Q2 - full k3s/k8s protocol conformance (do not reinvent the API).
- Q4 - built-in Router as a first-class platform primitive (openresty Rust
  port), not an addon.
- Q5 - M1 vertical slice de-risks the Router first (Ingress->Lua->real
  traffic).
- Q10 - JSON-only wire format for v1 (no protobuf).
- Q12/Q13/Q14 - Router VM model, coroutine<->async bridge, per-phase
  coroutines and filter semantics.

Read `plans/init-pro/decisions.md` for the full list (Q1-Q19).
