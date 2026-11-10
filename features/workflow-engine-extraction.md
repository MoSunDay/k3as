# workflow-engine-extraction (T7.3, Q42)

The argo-workflows model and engine live OUT of tree in the pinned
`MoSunDay/arust-workflow` workspace, referenced as the 8th link_repos
path-dep pin (the Q38 mechanism) - one codebase, one home. k3as keeps
thin adapters only; every embedded plane (openrusty Q32, rdb Q33,
flannel Q34, opencoder Q35, anyhome/nfsserve Q36, workflow Q42) is now
a reference, zero double-write. Decision: Q42; card: T7.3; detail in
`plans/init-pro/plan/07-agent-scheduling.md`.

## Shape

- `link_repos/arust-workflow` @ `687ed325` (in `link_repos.lock`):
  `workflow-model` (pure serde types - the 1:1 typed argoproj.io port)
  + `workflow-engine` (plan/dag/phase/status/pod/agent/cron_expr and
  the Workflow/CronWorkflow reconcilers as pure functions over a small
  `Kube` seam). 79 upstream unit tests, clippy clean, in that repo.
- `crates/api/src/argoproj.rs` (39 lines) replaces the 17-module
  in-tree tree: `pub use workflow_model::*` + the kept `register()` /
  `GROUP` / `VERSION`. `api::argoproj::*` consumer paths and the Q10
  JSON wire are unchanged; `tests/argoproj_fidelity.rs` fixtures are
  byte-identical across the move.
- `crates/controllers/src/controllers/workflow_bridge.rs` (151 lines):
  the engine's `Kube` seam over the in-process Client (Q19) with
  bidirectional lossless error mapping (Conflict / AlreadyExists /
  NotFound) + contract-drift tests. `workflow.rs` (31) and
  `cronworkflow.rs` (23) are thin adapters; runner/informer wiring
  untouched.
- Same sprint, `link_repos.lock`: any-home bumped a1e2d9c -> 42b8d8e
  (upstream e2e/docs tip, zero patch delta).

## Same-sprint fixes that rode along

- Router hot-reload REAL bug: `plugins_noop()` pointed at a
  NON-EXISTENT dir - openrusty's reload `discover` hard-fails
  `ReloadError::Io` on a missing dir (only bootstrap tolerates it), so
  every Ingress recompile on a fresh host died and the gateway 404ed
  forever. Fix: `create_dir_all` the dedicated empty dir on demand
  (bare `/tmp` stays forbidden - foreign `*.wasm` junk). Both
  previously-red tests green.
- Q41 breakage wiring completed: `router/endpoints.rs::parse_service`
  reads `status.loadBalancer.ingress[0].ip` (empty-string filtered)
  and `cli/runtime.rs` forwards `ServerBind.disable_servicelb` ->
  `ManagerConfig` - both were referenced-but-unwired at the Q41
  commit and broke the build.

## Evidence

- Landing tree: `cargo test --workspace --locked` 984 passed / 0
  failed; current tree 991 / 0. `cargo clippy --workspace
  --all-targets -- -D warnings` zero. `scripts/golden-conformance.sh`
  73 passed / 0 failed, exit 0 (incl. the argoproj G27-G29 suites).
- Upstream suite green in `link_repos/arust-workflow` itself;
  fidelity fixtures byte-identical across the extraction boundary.

## Limits / next steps

- Pin bumps flow through `link_repos.lock` +
  `scripts/bootstrap-link-repos.sh` (cold clones materialize before
  compiling); no runtime version negotiation needed - a path-dep is
  compile-time lockstep by construction.
- The upstream repo is the reference home for other consumers; k3as
  must never re-inline engine or model code (the one-codebase rule).
