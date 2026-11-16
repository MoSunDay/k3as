#!/usr/bin/env bash
# Q48 / T4.11: the engine process must stay single-threaded. clone3 with
# child_stack=0 is fork-like, so an async runtime (or any thread spawn)
# inside the engine or nsrun closure is a correctness bug, not a perf choice.
# This gate checks the dependency closure and the source tokens.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

fail=0
ok() { echo "OK   $*"; }
bad() { echo "FAIL $*"; fail=1; }

# (1) Dependency closure: no async runtime / thread pool crates.
TREE="$(cargo tree -p engine -e normal --prefix none 2>/dev/null)" || {
  bad "(1) cargo tree -p engine failed"; TREE=""; }
BAD_DEPS="$(printf '%s\n' "$TREE" | grep -E '^(tokio|mio|rayon|async-std|smol|futures-executor|threadpool|num_cpus|async-global-executor|blocking) ' || true)"
if [[ -z "$BAD_DEPS" ]]; then
  ok "(1) engine closure carries no async/thread-pool crates"
else
  bad "(1) engine closure pulls threaded crates:"; echo "$BAD_DEPS" | sed 's/^/      /'
fi
for dep in nsrun engine-proto; do
  if printf '%s\n' "$TREE" | grep -q "^$dep v0.2.0"; then
    ok "(1) engine closure includes $dep"
  else
    bad "(1) engine closure is missing $dep"
  fi
done

# (2) Source tokens: no runtime/thread spawning in the engine, its protocol
#     or nsrun. `std::thread::sleep` is allowed (it does not create threads).
SRC_HITS="$(grep -rnE 'thread::spawn|std::thread::Builder|tokio::|async_std::|rayon::|smol::' \
  crates/engine/src crates/engine-proto/src crates/nsrun/src || true)"
if [[ -z "$SRC_HITS" ]]; then
  ok "(2) no thread/async spawn tokens in engine sources"
else
  bad "(2) threaded/async tokens in the engine closure:"; echo "$SRC_HITS" | sed 's/^/      /'
fi

# (3) The engine crate's manifest declares only the two internal deps.
DEPS="$(awk '/^\[dependencies\]/{f=1;next}/^\[/{f=0}f && NF{print $1}' crates/engine/Cargo.toml | sed 's/=.*//' | sort | tr '\n' ' ')"
if [[ "$DEPS" == "engine-proto nsrun " ]]; then
  ok "(3) crates/engine depends only on engine-proto + nsrun"
else
  bad "(3) unexpected crates/engine dependencies: ${DEPS:-<none>}"
fi

# (4) Dispatch stays pre-clap/pre-tokio in the multicall binary.
if grep -q 'Some("engine" | "engine-run")' crates/init-pro/src/main.rs; then
  ok "(4) init-pro dispatches engine before clap"
else
  bad "(4) crates/init-pro/src/main.rs no longer intercepts engine pre-clap"
fi

echo "----"
if [[ "$fail" -eq 0 ]]; then
  echo "engine-singlethread-check: PASS (Q48 single-thread invariant)"
else
  echo "engine-singlethread-check: FAIL"
fi
exit "$fail"
