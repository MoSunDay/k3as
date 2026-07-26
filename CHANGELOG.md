# Changelog

All notable changes to init-pro are recorded here. Format is loosely
[Keep a Changelog](https://keepachangelog.com/); versions follow the plan
milestones in `plans/init-pro/`.

Test counts cited below are the **fresh** `cargo test --workspace` output at
the time of the entry (passed / failed), included so the numbers stay auditable.

## [Unreleased] — Phase 1, Sprint 3 (T1.2a)

### Added
- **T1.2a — HTTP discovery API server.** `init-pro server` now binds
  `127.0.0.1:6443` (overridable) and serves byte-correct Kubernetes API
  discovery over HTTP: `GET /api` (`APIVersions`), `GET /apis`
  (`APIGroupList`), `GET /api/v1` + `GET /apis/{group}/{version}`
  (`APIResourceList`), with `Content-Type: application/json` (Q10) and `404`
  for unknown group/version. This proves the HTTP framework choice,
  exercises T1.1's discovery builders over a real transport, and shares the
  stack with the Router data plane (T5.2). No etcd needed; discovery is driven
  entirely by the `SchemaRegistry`.
  - **Decision Q11 (HTTP framework & TLS posture).** Framework is **axum**
    (on hyper/tokio/tower) — one stack for both the apiserver (T1.2) and the
    Router (T5.2). **Plain HTTP for this slice**; TLS (rustls) + real kubectl
    interop is deferred to T1.2b/T1.3. Acceptance is `curl` byte-equivalence
    + a Rust integration test, not kubectl (kubectl refuses plain HTTP).
  - **New crate `init-pro-apiserver`** (`lib.rs`/`discovery_handlers.rs`/
    `serve.rs`, all ≤104 lines). `init-pro-api` stays HTTP-free; the apiserver
    crate is the thin transport layer over the T1.1 pure builders.
    `serve(registry, addr, server_address, shutdown)` takes a generic
    shutdown future (kept decoupled from `init-pro-infra`).
  - **Listen flags.** `--bind-address` (default `127.0.0.1`) +
    `--https-listen-port` (default `6443`) added to `init-pro server` (k3s
    parity), with `INIT_PRO_*` env support; removed from the no-op strip set.
    `--disable-apiserver` keeps the port closed.
  - **Graceful drain.** The server is spawned and joined after the shared
    `Shutdown` token fires, so axum drains in-flight requests before exit.
  - **6 tests** in `init-pro-apiserver` (1 router-build + 5 HTTP fidelity:
    `/api`, `/apis`, `/api/v1`, `/apis/init-pro.io/v1`, unknown→404); **2
    parity scripts** (`scripts/apiserver-discovery-parity-test.sh` — real
    `init-pro server` + `curl`, `graceful-shutdown-test.sh` still green).
    Workspace total **195 green**; `cargo clippy --all-targets -- -D warnings`
    clean; all new files ≤104 lines; pure additive diff (no `#[ignore]`,
    no deleted tests).

### Changed
- **Workspace deps:** added `axum` 0.8 (`http1`,`json`,`tokio`,`macros`),
  `tower` 0.5; added `net` to `tokio` features.
- **`init-pro-cli`:** `runtime.rs` spawns the apiserver (was the T1.1
  placeholder `let _ = &schema;`); `ServerCmd` gained the bind flags;
  `lib.rs` parses the bind address. Snapshot `tests/snapshots/server-help.txt`
  + `cli-flag-parity-test.sh` updated for the two new wired flags.
- **`decisions.md`:** added **Q11** ADR.

## [Unreleased] — Phase 1, Sprint 3

### Added
- **T1.1 — resource model & API group schema.** A Kubernetes-faithful Rust
  resource model in a new `init-pro-api` crate, unlocking T1.2 (APIServer),
  T1.3 (auth), T2.2 (etcd data plane), and T5.4 (router).
  - **Decision Q10 (serialization).** v1 is **JSON-only** on every path — API
    wire, etcd storage, watch streams. protobuf is explicitly deferred (see
    `decisions.md` Q10). `kubectl`/`kube-rs`/`helm` negotiate JSON via
    discovery automatically; no client breakage.
  - **`kube-core` 4.2 + `k8s-openapi` 0.28 (`v1_32`).** Cold build ~11s, 754MB
    RAM (kept behind `init-pro-api` so cli/infra never recompile it).
  - **S2 — GVK/GVR (`gvk.rs`).** `ApiVersion` round-trip parsing (core vs
    grouped, rejects empty version), GVK↔GVR join helpers, `TypeMeta`→GVK.
  - **S3 — schema registry (`schema.rs`).** `SchemaRegistry` maps GVK→type info
    (kind/plural/list-kind/scope); lossless GVK↔GVR conversion; core/v1 types
    (Pod, ConfigMap, Secret, Service, Namespace, Node, Event) registered from
    their static `k8s_openapi::Resource` consts; case-insensitive group lookup.
  - **S4 — JSON round-trip fidelity (`serde_ext.rs` + `tests/json_fidelity.rs`).**
    Round-trip of Pod/ConfigMap/Namespace is asserted (a) idempotent
    (serialize twice → identical bytes) and (b) semantically lossless
    (canonical-key-sorted compare). `canonical_json` for order-insensitive eq.
  - **S5 — Strategic Merge Patch (`patch.rs`).** Core SMP semantics: recursive
    map merge, `null`-deletes-field, merge-by-key lists (containers /
    initContainers / ephemeralContainers / volumes by `name`, ports by
    `containerPort`, env by `name`) with order preservation + `$patch: delete`,
    atomic replace for non-keyed lists; RFC 6902 JSON Patch fallback.
  - **S6 — `init-pro.io/v1` group (`initpro.rs`).** `LuaRouter` CRD (the Router
    config surface, Q4) with flattened `TypeMeta`/`ObjectMeta`/spec/status,
    registered into the schema registry, kubectl-apply JSON round-trip.
  - **S7 — discovery skeleton (`discovery.rs` + `init-pro-cli/discovery.rs`).**
    `/api` (`APIVersions`) + `/apis` (`APIGroupList`) + per-group
    `APIResourceList` bodies built from the registry — byte-correct today,
    served by T1.2's HTTP layer later. `init-pro server` builds the served
    schema + logs the group summary at startup.
  - **47 tests in `init-pro-api`** (8 gvk + 8 schema + 5 serde + 11 patch + 6
    discovery + 4 initpro + 5 json_fidelity integration) + 1 in `init-pro-cli`;
    workspace total **189 green**; `cargo clippy --workspace --all-targets
    -- -D warnings` clean; all files ≤400 lines.
  - **Structural coverage.** Every new `pub fn` on the resource model has a
    direct test: `get_by_gvr` (pod resolve + unknown → `None` + case-insensitive
    group) and `is_core_resource` (core `""` vs grouped); the trivial
    `with_merge_key` (merge-vs-replace), `to_json_pretty`/`canonical_value` and
    `is_empty` accessors are covered too — no untested public surface.

### Changed
- **`Cargo.toml` workspace deps:** added `init-pro-api`, `serde_json`,
  `thiserror`, `kube-core` 4.2 (features `json-patch`), `k8s-openapi` 0.28
  (`v1_32`), `json-patch` 4.
- **`00-foundation.md`:** T0.4 status corrected to `done` (was stale
  `not-started` despite being complete in Sprint 2) with evidence.
- **`decisions.md` / `README.md`:** added **Q10** ADR (JSON-only v1) + table row.
- **`index.md` SSOT:** T1.1 → `done`.

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
  - **B2 — zstd embed codegen (Q6).** `init-pro-vendor` compresses each acquired
    artifact per-file with zstd level 19 and emits a content-addressed
    `assets.rs` (one `include_bytes!` blob per file, keyed by SHA-256). Build
    embeds the blobs into the single binary; verified via `init-pro stage
    --dry-run` listing every artifact with its SHA-256.
  - **B3 — dataverify manifests.** Emits `.sha256sums` + `.links` (ported from
    k3s `pkg/dataverify/dataverify.go`) so runtime staging can recompute +
    compare byte-for-byte; the manifest doubles as the single source for
    expected sizes/links.
  - **B4 — Q7 license gate + SPDX-2.3 SBOM.** `build.rs` collects each
    component's upstream `LICENSE`/`NOTICE` into `LICENSES/`, runs the
    allow-list gate (Apache-2.0/BSD/MIT/ISC — non-cleared artifact fails the
    build), and generates a SPDX-2.3 SBOM referenced by `stage --dry-run`.
  - **B5 — runtime stage() (k3s `extract()` parity).**
    `crates/init-pro-cli/src/stage.rs` mirrors k3s `cmd/k3s/main.go:259-375`:
    flock `<data-dir>/data/.lock` → write blobs to `<data-dir>/data/<HASH>/`-tmp
    → `dataverify` (recompute `.sha256sums`/`.links`) → atomic rename → symlink
    `data/current` → `<HASH>` (+ `data/previous` rollback); writes `bin/` +
    `bin/aux/` and clones CNI-plugin symlinks into a stable `data/cni/`.
  - **B6 — acceptance harness.** `scripts/stage-fresh-dir-test.sh` copies the
    binary into an empty dir, runs `init-pro stage` against a fresh
    `<data-dir>`, and asserts the staged tree matches `.sha256sums`/`.links`
    byte-for-byte, `data/current` points at the new `<HASH>`, child `PATH`
    leads with the CNI dir, and a re-run is idempotent — **8/8 assertions
    green**.

### Tests
- `cargo test --locked --workspace` → **141 passed; 0 failed** (fresh; up from 23 → 81 → 101 → 141).
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
