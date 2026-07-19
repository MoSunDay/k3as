# init-pro

A from-scratch Rust reimplementation of a **k3s-compatible**, fully
protocol-standard Kubernetes distribution, shipped as **one multicall binary**
and featuring a first-class **built-in Lua Router** (an openresty-style data
plane). Ingesting `argv[0]` selects behavior:

```
init-pro server | agent | kubectl | ctr | crictl | containerd | etcd
```

## Status

**Phase 1, Sprint 1** — workspace + multicall skeleton + infra crate (T0.1 + T0.3).
See `plans/init-pro/` for the SSOT plan and `plans/init-pro/phase-1-implementation.md`
for the sprint breakdown.

## Build & verify

```
cargo build --locked             # single binary: target/debug/init-pro
cargo test --locked              # unit + integration tests across all crates
scripts/multicall-selftest.sh    # every alias answers --help (T0.1)
scripts/graceful-shutdown-test.sh # server drains on SIGTERM (T0.3)
```

> `--locked` pins to `Cargo.lock` so CI/release builds don't silently pick up
> new patch versions of dependencies (deps are caret-only). The selftest
> scripts run a **pre-built** binary, so build with `cargo build --locked`
> before invoking them.

## Layout

| Crate | Role |
|-------|------|
| `init-pro` | the single binary; `argv[0]` dispatch |
| `init-pro-multicall` | alias table + reexec + peer stubs (T0.1) |
| `init-pro-cli` | clap CLI surface + subcommands (T0.4) |
| `init-pro-infra` | tracing, layered config, graceful shutdown (T0.3) |
| `init-pro-core` | shared domain primitives |

## License

Apache-2.0 (bundled peers retain their own licenses; SBOM lands with T0.2).
