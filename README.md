# init-pro

> A from-scratch **Rust** reimplementation of a **k3s-compatible**, fully
> protocol-standard Kubernetes distribution — shipped as **one multicall
> binary** with a first-class, built-in **Lua data plane** (an openresty-style
> router, not a sidecar).

**English** | [中文](README_zh.md)

![Rust](https://img.shields.io/badge/Rust-stable%20(1.89)-ce422b?logo=rust)
![Edition](https://img.shields.io/badge/Edition-2021-orange)
![License](https://img.shields.io/badge/License-Apache--2.0-blue)
![Status](https://img.shields.io/badge/Phase%201-done%20%C2%B7%20Phase%202%20WIP-green)
![Tests](https://img.shields.io/badge/tests-326%20passing-brightgreen)
![Golden](https://img.shields.io/badge/golden-12%2F12-brightgreen)

The basename of `argv[0]` selects behavior — deploy the **same binary** under
every alias and symlink deployment *just works* (decision **Q1**):

```
init-pro  server | agent | kubectl | ctr | crictl | containerd | etcd
```

That is the whole distribution in a single artifact.

---

## Why init-pro

| | k3s | init-pro |
|---|---|---|
| Language | Go | **Rust** (`#![forbid(unsafe_code)]`) |
| Binary model | one multicall binary | one multicall binary (**Q1**) |
| Ingress / data plane | Traefik (separate sidecar) | **built-in Lua router** — a first-class platform primitive, not an addon (**Q4**) |
| Wire format | protobuf + JSON | **JSON-only for v1** (**Q10**) |
| Storage | real etcd or `kine` (SQLite) | `StorageBackend` trait; embedded default + swappable backends (**Q17**) |
| Protocol fidelity | k8s conformance | **full k3s/k8s conformance** (**Q2**) |

The headline bet: a **programmable HTTP data plane** (openresty's model,
ported to Rust) lives *inside* the distribution and is de-risked **first**
(**Q5**), so routing/Ingress works on real traffic long before the control
plane is complete.

---

## Status

**Phase 1 is complete** (M0 foundation + M1 router slice) and **Phase 2 is in
progress** — the `storage` crate landed (T2.1) and the apiserver now serves
**real REST CRUD + watch** over the embedded store (T1.2b).

| Layer | TODO | What | Status |
|------:|:-----|------|:------:|
| 0 | T0.1–T0.6 | multicall, vendor/bundle, infra, CLI, golden conformance | ✅ done |
| 1 | T1.1 | resource model + API-group schema + discovery builders | ✅ done |
| 1 | T1.2 | APIServer: discovery (T1.2a) + REST CRUD & watch (T1.2b) done; SSA (T1.2c) deferred | 🟡 in-progress |
| 1 | T1.3 | authn/authz (kubeconfig / token / RBAC) | ⬜ |
| 2 | T2.1 | embedded storage backend + `StorageBackend` trait (Q17) | ✅ done |
| 2 | T2.2 | storage data plane wired into apiserver | 🟡 in-progress |
| 2 | T2.3 | SQLite / KINE alternative backend | ⬜ |
| 3 | T3.1–T3.4 | controller-manager, scheduler, bootstrap/certs, HA | ⬜ |
| 4 | T4.1–T4.5 | containerd/CRI, kubelet, CNI, netpol, node tunnel | ⬜ |
| 5 | T5.1–T5.4 | Lua bridge, phase pipeline, `resty.*`, Ingress→route + TLS | ✅ done |
| 5 | T5.5–T5.7 | hot reload, ServiceLB, router-as-config-var | ⬜ |
| 6 | T6.1–T6.2 | HelmChart auto-deploy, standard addons | ⬜ |
| 7 | T7.1–T7.3 | AI-agent workloads, GPU scheduling, argo-workflows | ⬜ |

> The critical path is **T0.1 → T0.2 → T2.1 → T2.2 → T1.2 → T3.1/T3.2 → T4.2**.
> **T0.6 (golden conformance)** is a merge gate, not a node: every TODO must
> keep it green. See [`plans/init-pro/index.md`](plans/init-pro/index.md) for
> the authoritative SSOT and [`CHANGELOG.md`](CHANGELOG.md) for per-sprint
> retrospectives (326 tests passing, 0 failing; golden harness 12/12).

Today the server is a **functional data plane**: API discovery + REST CRUD
(create/list/get/replace/delete/patch) + a chunked watch stream, all backed by
the embedded store with resourceVersion CAS. Next gate: server-side apply
field-manager (T1.2c) and authn/authz (T1.3).

---

## Quick start

```bash
# 1. Build the single multicall binary (no network; empty embed registry)
cargo build --workspace --locked
#   -> target/debug/init-pro

# 2. Run the server (discovery + REST CRUD + watch + Lua data plane)
target/debug/init-pro server --https-listen-port 6443

# 3. Every alias works via symlink / argv[0]:
ln -sf init-pro kubectl && ./kubectl --help          # T0.1 multicall
target/debug/init-pro agent --help                   # agent subcommand
```

### Verify the build

```bash
cargo test  --workspace --locked                       # 326 tests
cargo clippy --workspace --all-targets -- -D warnings  # zero warnings
cargo fmt   --all --check                              # clean
```

### Acceptance / e2e scripts

These run a **pre-built** binary, so build first:

```bash
scripts/multicall-selftest.sh              # every alias answers --help (T0.1)
scripts/cli-flag-parity-test.sh            # k3s flag parity (T0.4)
scripts/graceful-shutdown-test.sh          # SIGTERM drains cleanly (T0.3)
scripts/router-coroutine-selftest.sh       # coroutine↔async bridge (T5.1)
scripts/apiserver-discovery-parity-test.sh # curl byte-parity vs k8s (T1.2a)
scripts/golden-conformance.sh              # immutable wire baseline (T0.6)
scripts/stage-fresh-dir-test.sh            # runtime stage() bundling (T0.2)
```

---

## The built-in Lua router (the headline feature)

A real openresty-style data plane — phase pipeline, `resty.*`, Ingress
compilation, reverse proxy, TLS — all **inside** the binary and drivable from
Lua via [`mlua`](https://crates.io/crates/mlua):

```
client request
   │
   ┌───▼──────────────────────────────────────────────┐
   │  Router VM (single mlua VM per worker, !Send)     │  Q12/Q13
   │  phase pipeline: rewrite → access → content →     │  Q14
   │    header_filter → body_filter → log  (+ init_worker)
   ├──────────────────────────────────────────────────┤
   │  ngx.*      req / header / status / var / exec   │
   │  resty.*    http · lock · random · string ·      │  T5.3
   │             lrucache · ngx.shared.DICT           │  Q15
   │  cosocket   ngx.socket.tcp (async bridge)        │
   ├──────────────────────────────────────────────────┤
   │  Ingress→Lua compiler → round-robin Balancer →   │  T5.4
   │  reverse proxy → upstream resolver               │
   │  TLS termination + SNI (rustls `ring`, Q16)      │
   │  hot-reload seam (no-restart config swap)        │
   └──────────────────────────────────────────────────┘
       │
   upstream pods
```

The coroutine↔async bridge (Q12) lets a Lua coroutine `yield` at a Rust
`await` point on a Tokio `LocalSet`, reproducing openresty's cooperative
concurrency in Rust. See [`features/router-data-plane.md`](features/router-data-plane.md).

---

## Workspace layout

10 crates, flat `src/*.rs` modules with `pub use` re-exports at the crate root:

| Crate | Role | Layer |
|-------|------|:-----:|
| [`init-pro`](crates/init-pro) | the single binary; `argv[0]` dispatch + `build.rs` asset bundling | 0 |
| [`multicall`](crates/multicall) | alias table + reexec + peer stubs | 0 |
| [`vendor`](crates/vendor) | pinned upstream-artifact acquire + SHA-256 verify + SPDX SBOM | 0 |
| [`infra`](crates/infra) | tracing, layered config, graceful shutdown | 0 |
| [`cli`](crates/cli) | clap CLI + subcommands + k3s flag-parity strip filter | 0 |
| [`common`](crates/common) | shared primitives: embed descriptor + `version()` | 0 |
| [`api`](crates/api) | resource model, schema registry, discovery builders, `init-pro.io` CRDs | 1 |
| [`apiserver`](crates/apiserver) | discovery + REST CRUD + watch over a `StorageBackend` (T1.2) | 1 |
| [`storage`](crates/storage) | `StorageBackend` trait + embedded etcd-compatible store | 2 |
| [`router`](crates/router) | built-in Lua data plane: phase pipeline, `resty.*`, Ingress→route, balancer, proxy, TLS | 5 |

---

## CI & container

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs on every
push/PR: **fmt `--check` → clippy `-D warnings` → `cargo test --locked` → debug
build → the full e2e suite**, plus a separate **`INIT_PRO_EMBED=1` bundle job**
(`stage-fresh-dir-test.sh`).

A multi-stage [`Dockerfile`](Dockerfile) builds the single binary in a slim
image (rust:1.89 builder → debian:bookworm-slim runtime) with every multicall
symlink installed:

```bash
docker build -t init-pro .                 # add --build-arg EMBED=1 to bake peers
docker run --rm -p 6443:6443 init-pro server
```

---

## Build flags (`build.rs`)

| Flag | Effect |
|------|--------|
| *(default)* | **no network** + **empty embed registry** — fast dev loop & `cargo test` |
| `INIT_PRO_VENDOR=1` | download pinned upstream peers (`containerd`/`etcd`/…) |
| `INIT_PRO_OFFLINE=1` | forbid network entirely (CI/release hardening) |
| `INIT_PRO_EMBED=1` | bake downloaded blobs into the binary (release image) |

`--locked` is mandatory for CI/release; dependencies are caret-only.

---

## Architecture decisions (ADRs)

17 locked decisions in [`plans/init-pro/decisions.md`](plans/init-pro/decisions.md).
The load-bearing ones:

| ADR | Decision |
|-----|----------|
| **Q1** | single multicall binary (`argv[0]` dispatch) |
| **Q2** | full k3s/k8s protocol conformance (do not reinvent the API) |
| **Q4** | the Router is a first-class platform primitive, not an addon |
| **Q5** | de-risk the Router *first* (M1 vertical slice) |
| **Q10** | JSON-only wire format for v1 (no protobuf) |
| **Q12–Q14** | Router VM model, coroutine↔async bridge, per-phase coroutines |
| **Q16** | crypto provider = `ring` (not `aws-lc-rs`) |
| **Q17** | storage = pure-Rust embedded store behind a trait (not etcd FFI) |

---

## Repository-local memory for agents

If you are an AI coding agent working in this repo, start here:

- [`agents.md`](agents.md) — entry point: SSOT pointers, build gates, conventions, state.
- [`features/`](features/) — feature cards (`discovery-api`, `router-data-plane`,
  `storage-layer`) + a feature-centric changelog.
- `plans/init-pro/` — the **SSOT**: `index.md` (status table), `plan/*.md`
  (per-layer detail), `decisions.md` (Q1–Q17).

**Never contradict the SSOT.** When you change semantics, update `index.md` +
the matching `plan/*.md` + `decisions.md` in lock-step.

---

## Hard conventions (enforced)

- `#![forbid(unsafe_code)]` in every lib crate.
- Pure-functional Rust idiom: `struct` + `impl` + traits is fine; no
  OOP inheritance.
- Flat `src/*.rs` with `pub mod` in `lib.rs`; sub-dirs use `mod.rs`; `pub use`
  re-exports at the crate root.
- File size: new files ≤ 400 lines, iterating files ≤ 800 lines.
- **JSON-only** wire format for v1 (no protobuf).
- No hardcoded secrets/API keys/tokens; no destructive DB ops without explicit
  permission. Comments reference task IDs (`T1.1`, `T5.4`) and decision codes
  (`Q10`, `Q17`).

---

## License

Apache-2.0. Bundled upstream peers retain their own licenses; an
[SPDX-2.3 SBOM](LICENSES/) is generated by the build (T0.2).
