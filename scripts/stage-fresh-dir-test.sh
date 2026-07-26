#!/usr/bin/env bash
# T0.2-B6 acceptance: runtime stage() against a fresh data dir.
#
# Builds with INIT_PRO_EMBED=1, stages into a temp dir, then verifies:
#   1. Every staged file matches its expected SHA-256 (recompute + compare)
#   2. data/current → <HASH> symlink exists
#   3. PATH entries include CNI dir (bin/aux) first
#   4. Re-running is idempotent (fast-path skip, no rewrite)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

pass=0; fail=0
ok()  { echo "OK   $1"; pass=$((pass+1)); }
bad() { echo "FAIL $1"; fail=$((fail+1)); }

DD="$(mktemp -d)"; trap 'rm -rf "$DD"' EXIT

# ---------------------------------------------------------------------------
# (0) Prerequisite: binary must be embedded.
# ---------------------------------------------------------------------------
if ! "$BIN" stage --dry-run 2>/dev/null | grep -q 'sha256='; then
  echo "error: $BIN has no embedded assets."
  echo "Run: INIT_PRO_EMBED=1 cargo build -p init-pro"
  exit 1
fi

# ---------------------------------------------------------------------------
# (1) Parse expected hashes from --dry-run output.
# ---------------------------------------------------------------------------
mapfile -t ENTRIES < <("$BIN" stage --dry-run 2>/dev/null | grep 'sha256=')
entry_count=${#ENTRIES[@]}
if [[ $entry_count -eq 0 ]]; then
  bad "dry-run produced 0 entries"
else
  ok "dry-run lists $entry_count embedded assets"
fi

# ---------------------------------------------------------------------------
# (2) Stage into a fresh dir.
# ---------------------------------------------------------------------------
STAGE_OUT="$("$BIN" stage -d "$DD" 2>&1)" || true
if echo "$STAGE_OUT" | grep -q "staged"; then
  ok "stage wrote assets to fresh data dir"
else
  bad "stage did not report writing assets"
  echo "$STAGE_OUT"
fi

# Extract the hash from "data/<hash>" in the output.
HASH=$(echo "$STAGE_OUT" | grep -oP 'data/\K[0-9a-f]{64}' | head -1)

# ---------------------------------------------------------------------------
# (3) data/current → <HASH> symlink.
# ---------------------------------------------------------------------------
CURRENT="$DD/data/current"
if [[ -L "$CURRENT" ]]; then
  TARGET=$(readlink "$CURRENT")
  if [[ "$TARGET" == "$HASH" ]]; then
    ok "data/current → $HASH"
  else
    bad "data/current → $TARGET (expected $HASH)"
  fi
else
  bad "data/current is not a symlink"
fi

# ---------------------------------------------------------------------------
# (4) Every staged file matches its expected SHA-256.
# ---------------------------------------------------------------------------
mismatch=0; checked=0
for entry in "${ENTRIES[@]}"; do
  # Parse: "  bin/runc  10802720 bytes  sha256=bcfc..."
  path=$(echo "$entry" | awk '{print $1}')
  expected=$(echo "$entry" | grep -oP 'sha256=\K[0-9a-f]{64}')
  staged="$CURRENT/$path"
  if [[ ! -f "$staged" ]]; then
    bad "missing staged file: $path"
    mismatch=1
    continue
  fi
  actual=$(sha256sum "$staged" | awk '{print $1}')
  checked=$((checked+1))
  if [[ "$actual" != "$expected" ]]; then
    bad "hash mismatch: $path (expected $expected, got $actual)"
    mismatch=1
  fi
done
if [[ $mismatch -eq 0 ]]; then
  ok "all $checked staged files match SHA-256 pins"
fi

# ---------------------------------------------------------------------------
# (5) Executable bits correct.
# ---------------------------------------------------------------------------
if [[ -x "$CURRENT/bin/runc" ]]; then
  ok "bin/runc is executable (0755)"
else
  bad "bin/runc is not executable"
fi
if [[ ! -x "$CURRENT/bin/aux/LICENSE" ]]; then
  ok "bin/aux/LICENSE is not executable (0644)"
else
  bad "bin/aux/LICENSE should not be executable"
fi

# ---------------------------------------------------------------------------
# (6) PATH entries: CNI dir (bin/aux) listed first.
# ---------------------------------------------------------------------------
if echo "$STAGE_OUT" | grep -A1 'PATH+' | head -1 | grep -q 'bin/aux'; then
  ok "PATH includes bin/aux (CNI) first"
else
  first_path=$(echo "$STAGE_OUT" | grep 'PATH+' | head -1)
  if echo "$first_path" | grep -q 'bin/aux'; then
    ok "PATH includes bin/aux (CNI) first"
  else
    bad "PATH does not list bin/aux first: $first_path"
  fi
fi

# ---------------------------------------------------------------------------
# (7) Idempotent re-run (fast-path skip).
# ---------------------------------------------------------------------------
REOUT="$("$BIN" stage -d "$DD" 2>&1)" || true
if echo "$REOUT" | grep -q "up-to-date"; then
  ok "re-run is idempotent (fast-path skip)"
else
  bad "re-run did not report up-to-date"
  echo "$REOUT"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "Results: $pass passed, $fail failed"
exit $([[ $fail -eq 0 ]] && echo 0 || echo 1)
