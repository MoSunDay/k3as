#!/usr/bin/env bash
# T0.3 acceptance: init-pro server drains within a deadline on SIGTERM.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }

DD="$(mktemp -d)"
trap 'rm -rf "$DD"' EXIT

# Start server, send SIGTERM after it's up, assert it exits 0 promptly.
"$BIN" server --data-dir "$DD" >/tmp/init-pro-server.log 2>&1 &
SRV=$!
# Wait until it logs readiness (or timeout).
for _ in $(seq 1 50); do
  grep -q "ready" /tmp/init-pro-server.log && break
  sleep 0.1
done

kill -TERM "$SRV"
deadline=$((SECONDS + 5))
code=-1
while (( SECONDS < deadline )); do
  if ! kill -0 "$SRV" 2>/dev/null; then
    wait "$SRV" || true
    code=$?
    break
  fi
  sleep 0.1
done

if (( code != 0 )); then
  echo "FAIL: server exited with $code (expected 0) or did not stop in 5s"
  cat /tmp/init-pro-server.log >&2 || true
  exit 1
fi
echo "OK   server drained on SIGTERM within deadline"
