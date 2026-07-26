# Changelog

All notable changes to init-pro are recorded here. Format is loosely
[Keep a Changelog](https://keepachangelog.com/); versions follow the plan
milestones in `plans/init-pro/`.

Test counts cited below are the **fresh** `cargo test --workspace` output at
the time of the entry (passed / failed), included so the numbers stay auditable.

## [Unreleased] — Phase 1, Sprint 2

### Added
- **T0.4 — k3s-compatible CLI.** The `server`/`agent` surface accepts the full
  k3s flag vocabulary in three postures (frozen matrix
  `plan/00-foundation-flag-matrix.md`, ADR Q9):
  - **Pre-clap config pre-scan (Q8, A1).** `infra/configfile.rs` parses the
    layered config file (`<data-dir>/config.yaml` by default) with two-pass
    resolution that breaks the data-dir↔config-path circularity (R3).
    `Config::resolve` now takes 3 args: `(cli_data_dir, cli_debug, cli_config)`.
    Precedence: CLI > env (`INIT_PRO_*`) > file > default. A `config_scan.rs`
    argv scanner finds `--config`/`-c` before clap, short-circuiting `--help`.
  - **17 accept-wired flags (Table A, A2).** `server`/`agent` clap-derive
    structs capture `--data-dir`/`-d`, `--debug`, `--config`/`-c`, `--disable`,
    `--disable-{etcd,apiserver,agent,controller-manager,scheduler,cloud-controller,kube-proxy,network-policy,helm-controller}`,
    `--datastore-endpoint`, `--prefer-bundled-bin`, `--token`/`-t`,
    `--server`/`-s`, `--cluster-init`.
  - **~108 accept-no-op-warn flags (Table C, A3).** `strip_noop()` removes
    them from argv before clap so operators' k3s scripts keep working;
    `warn_noops()` logs each distinct flag once at WARN (deduped).
  - **7 fatal conflict rules (Table B, A4).** `validate_server` /
    `validate_agent` enforce k3s-parity preconditions and emit matching
    messages before logging/resolve: cluster-reset-restore-path needs
    cluster-reset; disable-{apiserver,etcd} ✗ datastore-endpoint;
    disable-etcd needs server; unknown `--disable` token (whitelist:
    `coredns, servicelb, traefik, local-storage, metrics-server, runtimes`);
    agent needs token; agent needs server.
  - **Parity harness (A5).** `scripts/cli-flag-parity-test.sh` exercises all
    five matrix assertions (accept / no-op-warn / fatal / `INIT_PRO_*` parity /
    unknown `K3S_*` ignored). Frozen `server`/`agent --help` snapshots in
    `tests/snapshots/` gate the wired-flag surface.

- **T0.2 — packaging pipeline (in progress; B1 done, embed/stage/SBOM next).**
  - **B1 — pinned artifact acquire (Q6).** New `init-pro-vendor` crate
    (build-dependency of `init-pro`) reads `vendor/versions.toml` and acquires
    pinned upstream artifacts with SHA-256 verification (k3s `sha256sum -c`
    parity) into the gitignored `vendor/cache/` + `vendor/bin/`.
    - **Manifest** (`vendor/versions.toml`): containerd 1.7.20, runc 1.1.13,
      CNI plugins 1.5.1 — all Apache-2.0 (Q7 allow-list enforced at parse
      time; GPL `k3s-root` host utilities excluded from v1).
    - **Three acquire modes** (precedence OFFLINE > VENDOR > AUTO):
      `INIT_PRO_VENDOR=1` downloads missing artifacts; `INIT_PRO_OFFLINE=1`
      forbids network and requires a pre-populated cache (air-gap); the
      default Auto mode uses the cache if present else skips (so `cargo build`/
      `cargo test` stay network-free and fast). Pure `plan()` + offline
      contract covered by unit + integration tests.
    - **`crates/init-pro/build.rs`** drives acquire via the vendor crate and
      emits `cargo:rerun-if-{changed,env-changed}` directives.
    - Verified end-to-end: `INIT_PRO_VENDOR=1 cargo build` downloads + verifies
      + stages all three (containerd+runc→`vendor/bin/`, CNI→`vendor/bin/aux/`);
      a corrupt partial download was correctly rejected by the SHA-256 gate.

### Tests
- `cargo test --locked --workspace` → **101 passed; 0 failed** (fresh; up from 23 → 81 → 101).
  - `init-pro-cli` unit: +58 (config-file parse/resolve_path/scalar/slice/key+
    append; config_scan; strip_noop incl. short/value/dedup; conflicts ×7;
    help-parity surface).
  - `init-pro-infra` unit: config-file + 3-arg resolve coverage.
- `cargo clippy --workspace --all-targets -- -D warnings` → **0 warnings**.
- e2e: `scripts/cli-flag-parity-test.sh` → **16/16 assertions green**.

### Known limitations
- `run_server` / `run_agent` / `run_stage` remain Phase 1 stubs (idle until
  signal / manifest print) — real Layers 1–4 arrive from Phase 2.
- Config-file layer is read but not yet surfaced to `--dry-run` beyond
  `data-dir`; structured fields land with the layers that consume them.
- **T0.2 remaining:** B1 acquires only — the per-file zstd embed (`assets.rs`),
  the `.sha256sums`/`.links` runtime manifest (B3), the SPDX SBOM + license
  notice tree (B4, Q7), and the runtime `stage()` / `extract()` (B5) land in
  subsequent commits. `vendor/bin/` + `vendor/cache/` are gitignored build
  outputs, not committed.

## [Unreleased] — Phase 1, Sprint 1

### Added
- **T0.1 — multicall skeleton.** Single `init-pro` binary selects behavior from
  `argv[0]`. Alias table covers `init-pro`, `init-pro-server`, `init-pro-agent`,
  `server`, `agent`, `kubectl`, `ctr`, `crictl`, `containerd`, `etcd`. Unknown
  names fall through to the top-level CLI (clap help). Bundled-peer aliases
  answer `--help` with exit success and reject other args with exit `2` + a
  clear not-yet-implemented message (peers arrive with T0.2/T0.4).
- **T0.3 — infra crate (spike).** `tracing` init with k3s `--debug` parity
  (`RUST_LOG` always overrides); layered config (CLI > env > file > default)
  honoring `--data-dir`/`--debug` and `INIT_PRO_*` envs; graceful-shutdown
  coordination on SIGTERM/SIGINT.
- Cargo workspace with five crates: `init-pro`, `init-pro-multicall`,
  `init-pro-cli`, `init-pro-infra`, `init-pro-core`. `release` profile targets
  a single small stripped binary (Q1: single-binary constraint).

### Tests
- `cargo test --workspace` → **23 passed; 0 failed** (fresh).
  - `init-pro-multicall` unit: **9** (alias resolution incl. cross-wire guard,
    `Action::as_str` round-trip, basename/case, `wants_help`, external flag).
  - `init-pro-infra` unit: **12** (config precedence x5, signal trigger/idle,
    install-returns-ok, `logging::init` idempotency x4).
  - `init-pro` integration: **2** (`external_stub` help branch exits success +
    banner; no-help branch exits `2` + stderr, via real argv[0] dispatch).
- `cargo clippy --workspace --all-targets -- -D warnings` → **0 warnings**.
- `cargo build --workspace` → clean; release yields a single stripped binary.
- e2e (manual / CI): `scripts/multicall-selftest.sh` (T0.1),
  `scripts/graceful-shutdown-test.sh` (T0.3).

### Known limitations
- Bundled peers (`kubectl`/`ctr`/`crictl`/`containerd`/`etcd`) are stubs until
  T0.2 (bundling) + T0.4 (CLI parity).
- Config-file pre-scan layer returns `None` today; lands with T0.4.
