#!/usr/bin/env bash
# T5.1 acceptance (★kill-criterion★): prove a Lua coroutine yields at a Rust
# `await` point on the Tokio runtime, letting another coroutine run concurrently
# on the same worker VM — i.e. the mlua coroutine<->async bridge is real, not a
# fake that blocks the worker thread. Mirrors the script in
# plans/init-pro/plan/05-ingress-lua.md.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

ok()  { echo "OK   $*"; }
bad() { echo "FAIL $*" >&2; exit 1; }

# Kill-criterion: A sleeps long, B sleeps short; B must start INSIDE A's sleep
# window (concurrent), and the 10-coroutine scaling test must complete in ~max,
# not ~sum. See crates/router/tests/concurrency.rs.
cargo test -p router --test concurrency -- --nocapture \
  >/tmp/router-concurrency.log 2>&1 \
  || { cat /tmp/router-concurrency.log; bad "concurrency test failed"; }

grep -q "coroutine_yields_on_async_sleep ... ok" /tmp/router-concurrency.log \
  || bad "kill-criterion (yield-on-await) did not pass"
ok "Lua coroutine yields at await point (concurrency proven)"

grep -q "many_coroutines_scale_to_max_not_sum ... ok" /tmp/router-concurrency.log \
  || bad "scaling test did not pass"
ok "10 coroutines scale to ~max, not ~sum"

# Latency baseline: ngx.sleep(10ms) round-trip must be in tolerance.
cargo test -p router --test sleep_latency -- --nocapture \
  >/tmp/router-latency.log 2>&1 \
  || { cat /tmp/router-latency.log; bad "latency test failed"; }
ok "ngx.sleep latency baseline within tolerance"

echo "PASS router coroutine<->async bridge (Q4 de-risked)"
