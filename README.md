# init-pro

A from-scratch Rust reimplementation of a **k3s-compatible**, fully
protocol-standard Kubernetes distribution, shipped as **one multicall binary**
and featuring a first-class **built-in Lua Router** (an openresty-style data
plane). Ingesting `argv[0]` selects behavior:

```
init-pro server | agent | kubectl | ctr | crictl | containerd | etcd
```

## Status

**Phase 1 (M0 + M1) — complete.** M0 foundation is green (T0.1–T0.6, with the
golden-conformance merge gate running in CI); the M1 Router spike is de-risked
(T5.1–T5.4: an Ingress compiles to a route table and serves real traffic over
HTTP/HTTPS with no-restart hot reload). The Phase 2 entry point is TBD. See
`plans/init-pro/` for the SSOT plan and `plans/init-pro/phase-1-implementation.md`
for the sprint breakdown.

## Build & verify

```
cargo build --locked             # single binary: target/debug/init-pro
cargo test --locked              # unit + integration tests across all crates
scripts/multicall-selftest.sh     # every alias answers --help (T0.1)
scripts/graceful-shutdown-test.sh # server drains on SIGTERM (T0.3)
scripts/golden-conformance.sh     # T0.6 golden gate: empty-cluster baseline (6 cases)
scripts/cli-flag-parity-test.sh   # k3s CLI flag parity (accept/no-op/fatal)
```

> `--locked` pins to `Cargo.lock` so CI/release builds don't silently pick up
> new patch versions of dependencies (deps are caret-only). The selftest
> scripts run a **pre-built** binary, so build with `cargo build --locked`
> before invoking them.

## Layout

| Crate | Role |
|-------|------|
| `init-pro` | the single binary; `argv[0]` dispatch + `build.rs` (vendored acquire/embed/SBOM) |
| `common` | shared domain primitives |
| `multicall` | alias table + reexec + peer stubs (T0.1) |
| `cli` | clap CLI surface + subcommands + runtime `stage()` (T0.4, T0.2-B5) |
| `infra` | tracing, layered config, graceful shutdown (T0.3) |
| `vendor` | pinned-artifact acquire (SHA-256 gate), per-file zstd embed, license gate + SPDX SBOM (T0.2) |
| `api` | resource model, API group schema, GVK, strategic-merge patch (T1.1) |
| `apiserver` | HTTP discovery API server — axum (T1.2a) |
| `router` | built-in Lua Router — mlua bridge, phase pipeline, `resty::*` stdlib, Ingress→route compiler, balancer, reverse proxy, TLS (T5.1–T5.4) |

## License

Apache-2.0 (bundled peers retain their own licenses; SBOM lands with T0.2).
