# Layer 0 — Foundation

Mirrors `index.md` TODO IDs **T0.1–T0.6**. Edit in lock-step with `index.md`.

This layer establishes the **single-binary contract (Q1)**, the build/bundle
pipeline, shared infrastructure, the k3s-compatible CLI surface (Q2), the
planning system itself (T0.5), and the **conformance golden gate (T0.6)**
that every later TODO must keep green.

---

## T0.1 — 单一二进制 multicall crate 骨架

- **目标 / Goal**
  One `init-pro` binary whose behavior is selected by `argv[0]` (multicall),
  mirroring k3s `bin/k3s` dispatch (`link_repos/k3s/cmd/k3s/main.go:166`).

- **核心实现 / Core implementation**
  - Cargo workspace; root crate `init-pro` produces the single binary.
  - Dispatch on `std::env::args().next()` basename → subcommand or alias.
  - Alias set (k3s parity): `kubectl`, `ctr`, `crictl`, `containerd`,
    `server`, `agent`, `etcd`, `kubectl` (also: `init-pro-server`, `init-pro-agent`
    as internal names). Unknown `argv[0]` → print help.
  - `reexec` helper: child processes re-invoke the same binary with a
    forced alias (k3s `stageAndRun`, `link_repos/k3s/cmd/k3s/main.go`).
  - Crate skeleton: `init-pro-core`, `init-pro-cli`, `init-pro-multicall` (dispatch),
    placeholders for later layers.

- **验收手段 / Acceptance**
  - `cargo build` → exactly one binary at `target/release/init-pro`.
  - Script: `for a in init-pro kubectl ctr crictl containerd server agent etcd;
    do ln -sf init-pro $a; ./$a --help >/dev/null && echo OK $a; done` — all OK.
  - Unit test asserting dispatch table is exhaustive over the alias set.

- **状态 / Status** — done
- **证据 / Evidence** — `cargo build --release` -> single `target/release/init-pro`; `scripts/multicall-selftest.sh` all OK; `init-pro-multicall` unit tests green (alias resolution incl. cross-wire guard, `Action::as_str` round-trip, exhaustive dispatch-table); `init-pro` integration test covers `external_stub` help branch (exit success + banner) and no-help branch (exit `2` + stderr) via real `argv[0]` dispatch.
- **卡点 / Blockers** — none
- **依赖 / Depends on** — —

---

## T0.2 — 构建与 bundling pipeline

- **目标 / Goal**
  Embed foreign subprocess binaries (containerd, runc, CNI multicall) into
  the single `init-pro` binary and stage them to a data dir at runtime —
  k3s `//go:embed` + `extract()` equivalent
  (`link_repos/k3s/pkg/data/data.go`, `cmd/k3s/main.go:259-375`). **Scope
  note (Q6):** etcd is *not* bundled here — in k3s it is an in-process Go
  library (`pkg/executor/embed/etcd/`); init-pro's etcd embed/FFI is T2.1.

- **核心实现 / Core implementation**
  - **Acquire (`build.rs`):** download pinned upstream artifacts with
    recorded SHA-256 into a gitignored `vendor/bin/` (containerd, runc, CNI
    multicall — Apache-2.0 per Q7). Versions pinned in a checked-in
    manifest (e.g. `vendor/versions.toml`); each download verified by
    `sha256sum -c` (k3s `scripts/download:30` parity).
    `INIT_PRO_OFFLINE=1` forbids all network and requires a pre-populated
    `vendor/bin/` (the CI/air-gap fallback, cf. risk R-vendor-network).
  - **Manifest + license gate (Q7):** emit `.sha256sums` and `.links`
    manifests (ported from `pkg/dataverify/dataverify.go`); collect each
    component's upstream `LICENSE`/`NOTICE` into `LICENSES/`, generate a
    SPDX-2.3 SBOM, and run the license allow-list gate
    (Apache-2.0/BSD/MIT/ISC) — any non-cleared artifact fails the build.
    GPL `k3s-root` utilities are **excluded** in v1.
  - **Embed:** per-file zstd compression (Q6; target level 19) → generated
    `assets.rs` via `include_bytes!` (Rust `go:embed` equivalent). One
    content-addressed blob per file, keyed by SHA-256.
  - **Stage (runtime, `stage()`):** mirror k3s `extract()`
    (`cmd/k3s/main.go:259-375`): flock `<data-dir>/data/.lock` → write
    blobs to `<data-dir>/data/<HASH>/`-tmp → `dataverify` (recompute
    `.sha256sums` + `.links`) → atomic rename → symlink
    `data/current -> <HASH>` (+ `data/previous` rollback). Writes
    `<data-dir>/bin/` and `<data-dir>/bin/aux/`; clones CNI-plugin
    symlinks into a stable `<data-dir>/data/cni/` (never overwriting user
    plugins — k3s `main.go:357-364` parity).
  - **PATH (k3s `cmd/k3s/main.go:218-237` parity):** CNI dir first;
    default = host PATH then `bin/aux`; `--prefer-bundled-bin` flips
    `bin/aux` ahead of host PATH. Child processes reexec the same binary
    (T0.1 `reexec`).
  - **Dry-run contract:** `init-pro stage --dry-run` prints one line per
    artifact (path, size, SHA-256, compression) + the SBOM reference;
    performs no writes.

- **验收手段 / Acceptance**
  - `cargo build --release` produces one `init-pro` binary embedding all
    artifacts; build emits `LICENSES/` + SPDX SBOM and the license gate is
    green.
  - `INIT_PRO_OFFLINE=1 cargo build` succeeds against a pre-populated
    `vendor/bin/` with no network.
  - `init-pro stage --dry-run` lists every embedded artifact + SHA-256
    (matches `.sha256sums`).
  - **`scripts/stage-fresh-dir-test.sh`** (specified here, implemented in
    act mode): copy the binary into an empty dir, run `init-pro stage`
    against a fresh `<data-dir>`, then assert the staged `<data-dir>/bin/`
    tree matches `.sha256sums` and `.links` byte-for-byte (recompute +
    compare), `data/current` points at the new `<HASH>`, and child `PATH`
    includes the CNI dir first. Re-running is idempotent (fast path:
    `bin/init-pro` exists -> no rewrite).

- **状态 / Status** — done
- **证据 / Evidence** — `init-pro-vendor` crate (acquire/manifest/digest/embed/dataverify/sbom), `crates/init-pro/build.rs` (acquire + embed + SBOM driver), `crates/init-pro-cli/src/stage.rs` (runtime stage()), `vendor/versions.toml` (containerd 1.7.20 / runc 1.1.13 / CNI 1.5.1, all Apache-2.0); `scripts/stage-fresh-dir-test.sh` (8/8 assertions green). MSRV bumped 1.80→1.89 (std File::lock).
- **卡点 / Blockers** — none (Q6 resolves packaging topology; Q7 resolves
    licensing/SBOM/size; deferred items are intentional: etcd FFI = T2.1,
    `k3s-root` GPL host utilities = later point-release, hard size-cap
    value = set when the real v1 bundle is first measured).
- **依赖 / Depends on** — T0.1

---

## T0.3 — 公共基础设施 crate (log/config/signal)

- **目标 / Goal**
  A `init-pro-infra` crate providing logging, structured config, signal
  handling, and shutdown coordination shared by all layers.

- **核心实现 / Core implementation**
  - `tracing` + `tracing-subscriber` with k3s-style log levels/debug flag
    (`--debug` parity, `link_repos/k3s/cmd/k3s/main.go:114`).
  - Config: layered (CLI flag → env → config file → defaults); `--data-dir`
    aware (k3s `-d/--data-dir`, main.go:129).
  - Async runtime: `tokio`; graceful shutdown via `CancellationToken` on
    SIGTERM/SIGINT; reexec-safe.
  - Feature flags per layer (cargo features) so the spike build (M1) can
    exclude heavy layers.

- **验收手段 / Acceptance**
  - Unit tests for config precedence + data-dir resolution.
  - Integration test: send SIGTERM, assert all spawned tasks drain within
    a deadline.

- **状态 / Status** — done
- **证据 / Evidence** — `init-pro-infra` crate: `config` (5 precedence tests), `signal` (`Shutdown` + `install`), `logging` (`tracing` + `--debug`; 4 idempotency/env tests asserting no-panic across debug true/false + `RUST_LOG` set/unset); `scripts/graceful-shutdown-test.sh` asserts server drains on SIGTERM within a deadline.
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T0.1

---

## T0.4 — CLI: multicall + k3s 兼容 flag

- **目标 / Goal**
  A k3s-compatible CLI surface (Q2): `init-pro server`/`agent` accept the
  full k3s flag vocabulary. The Phase-1 subset is wired; the rest is
  accepted with a deduped WARN; only contradictions are fatal (decision
  **Q9**; matrix in `plan/00-foundation-flag-matrix.md`).

- **核心实现 / Core implementation**
  - **Pre-clap config pre-scan (Q8):** port k3s `pkg/configfilearg`
    (`parser.go`, `defaultparser.go`) to a layer that runs before clap
    parse. Resolution: env `INIT_PRO_CONFIG_FILE` -> `--config`/`-c` ->
    default `<data-dir>/config.yaml`; config values injected after the
    command word so CLI wins; slice flags append; `key+` = append-to-slice;
    per-command invalid-flag stripping applied to `server` (agent left
    pass-through, k3s parity); `--help/-h/--version/-v` short-circuit.
    `.d/` dropins and http-config sources **deferred** (documented, not
    Phase-1).
  - **clap-derive flag groups:** `ServerCmd` and `AgentCmd` define the
    full server/agent flag set from the matrix; each flag is tagged
    `accept-wired` (honored), `accept-no-op-warn` (logged once per
    process, deduped), or subject to a `fatal` conflict rule. Env-var
    parity uses `INIT_PRO_*` (matrix "Env-var parity" section).
  - **`--disable` validation (`DisableItems`,
    `pkg/cli/cmds/stage.go:9`):** accept only `coredns, servicelb,
    traefik, local-storage, metrics-server, runtimes`; unknown token ->
    fatal *"unknown disable item `<x>`"*. v1 validates the set and records
    the selection (manifest controllers = T6.x).
  - **Conflict rules (ported verbatim from
    `pkg/cli/server/server.go:245-265`):** `--cluster-reset-restore-path`
    requires `--cluster-reset`; `--disable-apiserver` conflicts
    `--datastore-endpoint`; `--disable-etcd` conflicts
    `--datastore-endpoint`; `--disable-etcd` requires `--server`; agent
    requires `--token` (without cert) and `--server`. See matrix Table B.
  - **Multicall (T0.1):** external aliases (`kubectl`/`ctr`/`crictl`/
    `containerd`) bypass the flag groups and reexec the bundled binary.

- **验收手段 / Acceptance**
  - **`scripts/cli-flag-parity-test.sh`** (specified here, implemented in
    act mode): asserts (1) every flag in the matrix is accepted without
    "unknown flag" error given a type-correct value; (2) each accept-wired
    flag is honored (e.g. `--data-dir` changes the resolved dir,
    `--disable coredns` is recorded); (3) each fatal rule exits non-zero
    with the k3s-parity message; (4) accept-no-op-warn flags emit the
    deduped WARN and exit zero; (5) unknown `K3S_*` env vars are ignored.
  - Snapshot baseline: `init-pro server --help` and `init-pro agent --help`
    output diffed against a frozen file (cosmetic deltas whitelisted); the
    matrix is the authoritative behavior spec.

- **状态 / Status** — done
- **证据 / Evidence** — `init-pro-cli` clap-derive `ServerCmd`/`AgentCmd`
    (A2) wire the Phase-1 flag subset; `strip_noop()` + deduped `warn_noops()`
    (A3) accept the rest with WARN; `validate_server`/`validate_agent`
    (A4) enforce 7 fatal conflict rules; `scripts/cli-flag-parity-test.sh`
    (A5) exercises all five matrix assertions + frozen `tests/snapshots/`
    `server`/`agent --help` gate the wired-flag surface. 17 accept-wired
    flags, ~108 accept-no-op-warn, 7 fatal rules — all green.
- **卡点 / Blockers** — none (Q8 resolves config pre-scan scope; Q9 +
    `plan/00-foundation-flag-matrix.md` resolve the flag posture; `.d/`
    dropins & http-config are an intentional documented deferral).
- **依赖 / Depends on** — T0.1, T0.3

---

## T0.5 — 规划体系与文档中枢

- **目标 / Goal**
  The planning artifact set (`plans/init-pro/**`) that governs the
  rewrite — this very file set.

- **核心实现 / Core implementation**
  - `README.md` (goal/decisions/milestones), `index.md` (SSOT, 33 TODOs,
    status table, DAG), `decisions.md` (Q1–Q5 ADRs),
    `template/TODO.md` (7-field schema), `plan/00..07-*.md` (per-layer).
  - Convention: status changes MUST update `index.md` + the TODO's
    `证据`; TODO IDs identical across files.

- **验收手段 / Acceptance**
  - Lint script: every TODO ID in `index.md` appears exactly once in some
    `plan/*.md` and vice-versa; every TODO has all 7 fields.
  - DAG in `index.md` is acyclic (checked by script).

- **状态 / Status** — done
- **证据 / Evidence** — `plans/init-pro/**`
- **卡点 / Blockers** — none
- **依赖 / Depends on** — —

---

## T0.6 — 协议兼容性测试基线 (golden conformance)

- **目标 / Goal**
  An immutable **golden set** of k3s/k8s conformance + e2e cases that
  becomes the merge gate for every later TODO (Q2).

- **核心实现 / Core implementation**
  - Vendor a curated subset of upstream `kubernetes/e2e` + k3s acceptance
    tests as fixtures/scenarios (not the whole suite).
  - Driver harness boots `init-pro server` (+agent), runs real `kubectl`/
    `helm`/`kube-rs` clients, asserts wire-level results.
  - Categories: API round-trip, watch, etcd storage, scheduling, CRI,
    networking, Ingress/Router. Each later TODO tags the golden cases it
    must keep green.
  - Runs in CI on every change; flaky cases are quarantined, not deleted.

- **验收手段 / Acceptance**
  - Empty-cluster golden suite passes against a stub `init-pro` today (the
    harness itself is the deliverable); suite grows as layers land.
  - CI job `golden` is a required check.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — Licensing/attribution of upstream e2e fixtures;
    pick the smallest meaningful subset first.
- **依赖 / Depends on** — T0.1
