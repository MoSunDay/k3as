#!/usr/bin/env bash
# T0.1 acceptance: every documented multicall alias must answer --help with
# exit 0. Mirrors the script in plans/init-pro/plan/00-foundation.md.
set -euo pipefail

# Resolve the repo root from the script location.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

if [[ ! -x "$BIN" ]]; then
  echo "error: $BIN not found. Run 'cargo build' first." >&2
  exit 1
fi

# Work in a temp dir so the symlinks don't pollute the repo.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

aliases=(init-pro kubectl ctr crictl containerd server agent etcd)
fail=0
for a in "${aliases[@]}"; do
  ln -sf "$BIN" "$WORK/$a"
  if "$WORK/$a" --help >/dev/null 2>&1; then
    echo "OK   $a"
  else
    echo "FAIL $a (exit $?)"
    fail=1
  fi
done

# Unknown name should NOT crash; it prints help (clap) and exits 0.
ln -sf "$BIN" "$WORK/definitely-not-an-alias"
if "$WORK/definitely-not-an-alias" --help >/dev/null 2>&1; then
  echo "OK   (unknown -> help)"
else
  echo "FAIL unknown-alias should fall through to help"
  fail=1
fi

exit "$fail"
