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
- **证据 / Evidence** — `cargo build --release` -> single `target/release/init-pro`; `scripts/multicall-selftest.sh` all OK; `init-pro-multicall` dispatch-table unit tests green.
- **卡点 / Blockers** — none
- **依赖 / Depends on** — —

---

## T0.2 — 构建与 bundling pipeline

- **目标 / Goal**
  Embed foreign binaries (containerd, etcd, CNI plugins) into the single
  `init-pro` binary and stage them to a data dir at runtime — k3s `//go:embed`
  equivalent (`link_repos/k3s/pkg/data/data.go`, `pkg/deploy/stage.go`).

- **核心实现 / Core implementation**
  - `build.rs` downloads/pins upstream artifacts (containerd, etcd, CNI)
    into a vendored `vendor/bin/` (gitignored), hashes recorded.
  - Embed as compressed blobs (`include_bytes!` via a generated `assets.rs`,
    or `rust-embed`); k3s uses tar staging — init-pro uses per-file gzip to
    dedupe and keep image smaller.
  - Runtime: `stage()` writes blobs to `<data-dir>/bin/{,aux}` and sets
    `PATH` for child processes (k3s `stageAndRun` parity).
  - Reproducible builds: pin versions, `--locked`, record SBOM.

- **验收手段 / Acceptance**
  - `cargo build --release` produces one binary embedding all artifacts.
  - `init-pro stage --dry-run` lists every embedded artifact + SHA256.
  - Fresh-dir test: copy binary elsewhere, run `init-pro stage`, assert staged
    tree matches recorded hashes.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — Licensing/SBOM for bundled Go binaries (Apache-2.0
  notices); size budget target TBD.
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
- **证据 / Evidence** — `init-pro-infra` crate: `config` (5 precedence tests), `signal` (`Shutdown` + `install`), `logging` (`tracing` + `--debug`); `scripts/graceful-shutdown-test.sh` asserts server drains on SIGTERM within a deadline.
- **卡点 / Blockers** — none
- **依赖 / Depends on** — T0.1

---

## T0.4 — CLI: multicall + k3s 兼容 flag

- **目标 / Goal**
  A k3s-compatible CLI surface (Q2): `server`, `agent`, `kubectl`, `ctr`,
  plus flags `--data-dir/-d`, `--disable`, `--disable-etcd`,
  `--disable-apiserver`, `--disable-agent`, `--datastore-endpoint`,
  `--prefer-bundled-bin`, `--debug` — matching k3s `pkg/cli/cmds/*.go`.

- **核心实现 / Core implementation**
  - `clap` derive; top-level `init-pro` with subcommands `server`/`agent`;
    multicall aliases dispatch to wrapped external CLIs (T0.1).
  - `--disable` accepts k3s set: `coredns, servicelb, traefik,
    local-storage, metrics-server, runtimes` (k3s `DisableItems`,
    `link_repos/k3s/pkg/cli/cmds/stage.go`).
  - Flag validation parity: e.g. `--disable-apiserver` conflicts with
    `--datastore-endpoint` (k3s `pkg/cli/server/server.go:261`).
  - Config-file pre-scan for `--data-dir`/`--debug` before clap parse (k3s
    `configfilearg.MustFindString`).

- **验收手段 / Acceptance**
  - Snapshot test: `init-pro server --help` text diff against a frozen
    k3s-`server --help` baseline (whitelist cosmetic deltas).
  - Property test: every k3s documented flag is accepted or explicitly
    rejected-with-clear-error by init-pro.

- **状态 / Status** — not-started
- **证据 / Evidence** — —
- **卡点 / Blockers** — Must decide which k3s flags are no-ops vs fatal in
    v1 (track in a matrix under this TODO).
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
