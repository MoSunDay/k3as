# containerd-runtime (T4.1)

The node runtime bundle: Rust-native config templating, idempotent
vendor staging, and supervision of the bundled containerd behind the
multicall seam, plus a first-class `init-pro crictl` ops verb. Q24 is
the spike ADR, Q25 the as-built ADR, Q26 the CRI-client strategy
(`plans/init-pro/decisions.md`); as-built detail in
`plans/init-pro/plan/04-node.md`.

## Shape

- `crates/runtime` (pure functions, no OOP): `config.rs` renders
  containerd TOML v2 from `ContainerdConfigVars::for_data_dir` (CRI
  plugin enabled); `stage.rs` stages the vendored tree (containerd, ctr,
  shims, runc, crictl, `aux/` cni-plugins) idempotently by SHA-256 plus
  the CNI conflist `10-flannel.conflist` (stage.rs; the flannel
  meta-plugin chain that replaced the pre-T4.3 static bridge/host-local
  conflist, Q34);
  `supervisor.rs` runs the spawn/health/backoff/drain loop.
- k3s layout under the data dir: `agent/containerd/`,
  `agent/etc/containerd/config.toml`, `run/containerd/containerd.sock`.
  `start_agent_runtime` (defined in `crates/runtime/src/lib.rs`; called
  from the agent path in `crates/cli/src/runtime.rs`) composes stage ->
  render -> supervise. Since Sprint 28 the kubelet drains FIRST on
  shutdown (sandbox stop+rmp, then a bounded wait for CRI to report
  zero sandboxes) and only then does containerd go down
  (`crates/cli/src/runtime.rs`).
- Server keeps the runtime off by default; the single-node UX shipped
  in Sprint 24 (`scripts/local-up.sh`, promoted to the official UX).

## Supervision (Q25)

- Boot gate: socket health via UnixStream poll before "healthy".
- Backoff `base << restarts`, capped at 5 s; STABLE_AFTER 30 s resets
  the ladder (kill -9 rebirth is covered by integration test).
- Drain = SIGKILL + bounded 10 s wait. Deliberately **no SIGTERM**:
  containerd child-reaping is not guaranteed for a foreign runtime
  (k3s kills the tree too).
- `infra::signal::Shutdown` is **sticky** (fired flag +
  `Notified::enable`) — the old memoryless `notify_waiters` could lose
  wakeups during select gaps. Affects every Shutdown consumer, strictly
  more correct.

## Ops verbs (crictl / ctr)

- `init-pro crictl ...` / `init-pro ctr ...` are intercepted pre-clap
  (never touch the flag strip filter) and re-exec the staged peer with
  the agent endpoint injected (`--runtime-endpoint` / `--address`)
  unless the caller supplied one:
  `multicall::crictl_endpoint_args` / `ctr_address_args`.
- `crictl` v1.31.1 is vendored through the normal T0.2 pin/SHA-256
  pipeline (`vendor/versions.toml`), staged like every other peer.

## CRI client (Q26)

- Route B now: vendored-crictl subprocess (~20 ms/call, zero new deps)
  covers ps/pods/images/pull/run/exec incl. stdio passthrough.
- Route A later: native tonic gRPC measured sub-ms but costs a
  100-crate dep tree and needs vendored `cri-api` protos + protoc.
  Trigger is explicit: only when T4.2 needs in-process
  streaming/watch (sandbox events, pull progress, port-forward), as a
  leaf `cri-client` crate behind a feature flag.

## Env vars

- `INIT_PRO_SANDBOX_IMAGE` — overrides the CRI sandbox/pause image
  (default `registry.k8s.io/pause:3.10`).
- `INIT_PRO_VENDOR_BIN` — vendor bundle root override (else
  exe-relative `../../vendor/bin`, else cwd).
- `INIT_PRO_DATA_DIR` — data dir for the multicall peer path
  (`crictl`/`ctr`/`containerd` locate the staged tree + socket).

## Proof

- ~50 unit tests in `crates/runtime/` plus integration tests in two
  files: `tests/supervisor_integration.rs` (kill -9 rebirth + crictl
  round-trip over the live CRI socket) and `tests/cri_integration.rs`.
  Both SKIP, not fail, when `vendor/bin/containerd` is absent.
  15 multicall tests; 29 infra (sticky-shutdown regression).
- Golden G24 (`scripts/golden-conformance.sh`): agent supervises
  containerd, `crictl version`/`ps` round-trip over CRI; the
  sandbox-pull smoke SKIPs when the registry is unreachable (Q24
  egress note).
- Verify: `cargo test -p runtime -p multicall -p infra --locked` and
  `scripts/golden-conformance.sh` (build first).
