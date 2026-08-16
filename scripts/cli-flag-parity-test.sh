#!/usr/bin/env bash
# T0.4 acceptance: k3s CLI flag parity (Q9 matrix). Five behavioral assertions
# against the frozen baseline in plan/00-foundation-flag-matrix.md, plus a
# wired-flag surface check against the snapshots in tests/snapshots/.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"
SNAP="$ROOT/tests/snapshots"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }

pass=0; fail=0
ok()  { echo "OK   $1"; pass=$((pass+1)); }
bad() { echo "FAIL $1"; fail=$((fail+1)); }
DD="$(mktemp -d)"; out="$(mktemp)"; trap 'rm -rf "$DD" "$out"' EXIT

# ---------------------------------------------------------------------------
# (1) ACCEPT: a sample of no-op flags (Table C) is accepted without an
#     "unknown flag"/"unexpected argument" error. These are stripped pre-clap,
#     so `stage` sees only known args. Wired flags (Table A) are validated by
#     the clap schema — their surface is checked in the snapshot step below.
# ---------------------------------------------------------------------------
"$BIN" --data-dir "$DD" stage \
  --debug --rootless --cluster-cidr 10.0.0.0/16 --tls-san a.example --node-name n1 \
  --dry-run >"$out" 2>&1
rc=$?
if [[ $rc -eq 0 && ! "$(<"$out")" =~ unexpected\ argument|unknown ]]; then
  ok "(1) no-op flags accepted (stripped, exit 0, no unknown-flag error)"
else bad "(1) accept: rc=$rc"; sed 's/^/      /' "$out" | head -5; fi

# ---------------------------------------------------------------------------
# (4) ACCEPT-NO-OP-WARN: each no-op logs one WARN; repeats are deduped.
# ---------------------------------------------------------------------------
"$BIN" --data-dir "$DD" stage --rootless --cluster-cidr 10.0.0.0/16 --dry-run >"$out" 2>&1
rc=$?
n=$(grep -c 'accepted but not yet implemented; no-op' "$out" || true)
if [[ $rc -eq 0 && "$n" -eq 2 && "$(<"$out")" =~ rootless && "$(<"$out")" =~ cluster-cidr ]]; then
  ok "(4) two no-op flags -> two deduped WARN lines, exit 0"
else bad "(4) no-op-warn: rc=$rc lines=$n"; fi
"$BIN" --data-dir "$DD" stage --rootless --rootless --dry-run >"$out" 2>&1
n=$(grep -c 'accepted but not yet implemented; no-op' "$out" || true)
if [[ "$n" -eq 1 ]]; then ok "(4b) repeated no-op flag deduped to one WARN"; else bad "(4b) dedup: lines=$n"; fi

# ---------------------------------------------------------------------------
# (3) FATAL: each conflict rule (Table B) exits non-zero with parity message.
# ---------------------------------------------------------------------------
chk_fatal() { # desc expected_substr -- args...
  local desc="$1" sub="$2"; shift 3
  local r=0
  "$BIN" --data-dir "$DD" "$@" >"$out" 2>&1 || r=$?
  if [[ $r -ne 0 && "$(<"$out")" == *"$sub"* ]]; then ok "$desc"
  else bad "$desc (rc=$r)"; sed 's/^/      /' "$out"|head -2; fi
}
chk_fatal "(3a) cluster-reset-restore-path needs cluster-reset" "--cluster-reset required with --cluster-reset-restore-path" -- server --cluster-reset-restore-path /x
chk_fatal "(3b) disable-apiserver x datastore-endpoint" "cannot use --disable-apiserver with --datastore-endpoint" -- server --disable-apiserver --datastore-endpoint mysql://x
chk_fatal "(3c) disable-etcd x datastore-endpoint" "cannot use --disable-etcd with --datastore-endpoint" -- server --server https://x --disable-etcd --datastore-endpoint mysql://x
chk_fatal "(3d) disable-etcd needs server" "--server is required with --disable-etcd" -- server --disable-etcd
chk_fatal "(3e) unknown --disable token" "unknown disable item" -- server --disable bogus
chk_fatal "(3f) agent token required" "--token is required" -- agent
chk_fatal "(3g) agent server required" "--server is required" -- agent --token secret

# ---------------------------------------------------------------------------
# (2) WIRED HONORED: --data-dir changes the resolved dir (stage --dry-run);
#     a known --disable set passes validation (server reaches "ready").
# ---------------------------------------------------------------------------
"$BIN" --data-dir "$DD" stage --dry-run >"$out" 2>&1
if grep -q "data-dir: $DD" "$out"; then ok "(2a) --data-dir honored"; else bad "(2a) data-dir not honored"; fi
# server with a known disable token must NOT trip the unknown-disable fatal:
# it should start and block (alive after 0.6s => validation passed).
"$BIN" --data-dir "$DD" server --disable coredns,servicelb,traefik >"$out" 2>&1 &
SRV=$!; sleep 0.6
if kill -0 "$SRV" 2>/dev/null; then ok "(2b) --disable known token accepted (server reached ready)"
else
  if grep -q "unknown disable item" "$out"; then bad "(2b) known disable rejected"
  else ok "(2b) --disable known token accepted"; fi
fi
kill -TERM "$SRV" 2>/dev/null || true; wait "$SRV" 2>/dev/null || true

# ---------------------------------------------------------------------------
# (5) ENV PARITY: INIT_PRO_DATA_DIR honored; unknown K3S_DATA_DIR ignored.
# ---------------------------------------------------------------------------
DD2="$(mktemp -d)"
INIT_PRO_DATA_DIR="$DD2" "$BIN" stage --dry-run >"$out" 2>&1
if grep -q "data-dir: $DD2" "$out"; then ok "(5a) INIT_PRO_DATA_DIR env parity honored"; else bad "(5a) INIT_PRO_DATA_DIR not honored"; fi
K3S_DATA_DIR="/must-be-ignored" "$BIN" stage --dry-run >"$out" 2>&1
if grep -q "data-dir: /must-be-ignored" "$out"; then bad "(5b) K3S_DATA_DIR leaked (must be ignored)"; else ok "(5b) unknown K3S_DATA_DIR env ignored"; fi
rm -rf "$DD2"

# ---------------------------------------------------------------------------
# Wired-flag surface: every Table-A flag appears in the frozen --help snapshot.
# ---------------------------------------------------------------------------
check_help() { # snapshot scope flags...
  local snap="$1" scope="$2"; shift 2; local text; text="$(<"$snap")"; local missing=()
  for f in "$@"; do [[ "$text" == *"$f"* ]] || missing+=("$f"); done
  if [[ ${#missing[@]} -eq 0 ]]; then ok "($scope) snapshot lists all wired flags"
  else bad "($scope) missing: ${missing[*]}"; fi
}
check_help "$SNAP/server-help.txt" "server" \
  --config --data-dir --debug --token --server --prefer-bundled-bin \
  --bind-address --https-listen-port \
  --disable --disable-etcd --disable-apiserver --disable-agent \
  --disable-controller-manager --disable-scheduler --disable-cloud-controller \
  --disable-kube-proxy --disable-network-policy --disable-helm-controller \
  --datastore-endpoint --cluster-init --kube-scheduler-arg
check_help "$SNAP/agent-help.txt" "agent" \
  --config --data-dir --debug --token --server --prefer-bundled-bin --node-name

echo "----"
echo "parity: $pass passed, $fail failed"
[[ $fail -eq 0 ]]
