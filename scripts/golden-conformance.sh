#!/usr/bin/env bash
# T0.6 golden conformance: the immutable baseline of k3s/k8s wire-level
# behaviors that every later TODO must keep green. Boots a real init-pro
# server and diffs its responses against committed golden fixtures in golden/.
#
# Today = the EMPTY-CLUSTER baseline: discovery (T1.2a) + CRUD/watch over the
# embedded store (T1.2b), extended by the T3.1a controller acceptance (G17:
# Deployment scale converge + Endpoints membership) and the T3.1b suite
# (G18-G21: rolling update + kubectl rollout status, StatefulSet ordinal
# identity + PVCs, DaemonSet per-Node scheduling, GC cascade + namespace
# drain). The discovery contract is
# byte-stable; CRUD cases
# collection endpoints exist yet. As CRUD/watch/storage/scheduling layers land
# (T1.2, T2.x, T3.x), their golden cases are appended below — each TODO tags
# which cases it must keep green (plan/00-foundation.md T0.6, Q2 merge gate).
#
# Later suites appended below: storage semantics G02-G11 (T2.1/T2.2),
# control plane G12-G23 (T3.1/T3.2), the node layer G24-G26
# (T4.1 containerd supervision; T4.2 kubelet drives Deployment pods to
# Running+Ready over real containerd with the Q27 airgap pause image —
# G25 scope A lifecycle; G26 scope B: S1 probe gating/restart, S2
# emptyDir + configMap projection, S4 pod-name hostname, on the airgap
# echo workload; G24-G26 SKIP without the vendor bundle, G25/G26 also
# need cc), and the argoproj.io group G27-G29 (T7.3 Scope A:
# byte-stable Workflow/CronWorkflow discovery + CRUD/SSA lifecycle; Scope B
# G29: a two-step Workflow runs to Succeeded on the agent/containerd stack
# with the Q27 airgap wfstep image; G29 SKIPs like G25 without the vendor
# bundle or cc). Scope C (T4.2): G30 walks kubectl logs/exec through the
# apiserver relay -> kubelet local listener -> crictl subprocess chain
# (Q31 plain-HTTP single-shot; SKIPs like G25 without vendor bundle or cc).
#
# Volatility: the only non-deterministic field is APIVersions.serverAddress
# (the bind host:port); it is normalized to the token @@PORT@@ before diffing.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"
GOLDEN="$ROOT/golden"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl >/dev/null || { echo "error: curl is required" >&2; exit 1; }
command -v diff >/dev/null || { echo "error: diff is required" >&2; exit 1; }

ok()  { echo "OK   $*"; }

# Pick a free loopback port (avoids clashing with a host apiserver / prior run).
pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
    || echo $((20000 + RANDOM % 10000))
}
PORT="$(pick_port)"

DD="$(mktemp -d)"
LOG="$(mktemp)"
"$BIN" server --data-dir "$DD" --bind-address 127.0.0.1 --https-listen-port "$PORT" >"$LOG" 2>&1 &
SRV=$!
SRV2=""; STUB_PID=""
cleanup() {
  kill "$SRV" 2>/dev/null || true; wait "$SRV" 2>/dev/null || true
  [[ -n "$SRV2" ]] && { kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; }
  [[ -n "$STUB_PID" ]] && kill "$STUB_PID" 2>/dev/null || true
  [[ -n "${AGENT_PID:-}" ]] && { kill "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; }
  rm -rf "$DD" "$LOG"
}
trap cleanup EXIT

# Route fatal signals through the EXIT trap (bash skips it on untrapped signals).
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

for _ in $(seq 1 60); do grep -q "discovery listening" "$LOG" && break; sleep 0.1; done
grep -q "discovery listening" "$LOG" || { echo "error: server never reported listening"; cat "$LOG" >&2; exit 1; }

BASE="http://127.0.0.1:${PORT}"
PASS=0
FAIL=0

# Compare a live endpoint's body to its golden fixture.
#   $1 id   $2 path   $3 golden-file   $4 normalize: "serveraddr" | (else none)
check_body() {
  local id="$1" path="$2" golden_file="$3" norm="${4:-no}"
  local live
  live="$(curl -fsS "$BASE$path")" || { echo "FAIL $id  $path  (fetch failed)"; FAIL=$((FAIL+1)); return; }
  if [[ "$norm" == "serveraddr" ]]; then
    # Normalize the volatile bind address to the committed token.
    live="${live//127.0.0.1:$PORT/127.0.0.1:@@PORT@@}"
  fi
  local expected; expected="$(cat "$golden_file")"
  if [[ "$live" == "$expected" ]]; then
    ok "$id  $path"
    PASS=$((PASS+1))
  else
    echo "FAIL $id  $path  (body mismatch)"
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$live") | sed 's/^/    /' >&2
    FAIL=$((FAIL+1))
  fi
}

# Assert an endpoint returns an exact HTTP status.
check_status() {
  local id="$1" path="$2" want="$3" desc="$4"
  local code; code="$(curl -s -o /dev/null -w '%{http_code}' "$BASE$path")"
  if [[ "$code" == "$want" ]]; then
    ok "$id  $path -> $code  ($desc)"
    PASS=$((PASS+1))
  else
    echo "FAIL $id  $path -> $code (expected $want)  ($desc)"
    FAIL=$((FAIL+1))
  fi
}

# Assert an arbitrary HTTP method returns an exact status, optionally with a
# request body + content-type. Used for CRUD round-trips introduced by T1.2b.
#   $1 id   $2 method   $3 path   $4 want   $5 desc   [$6 content-type $7 body-file]
check_method() {
  local id="$1" method="$2" path="$3" want="$4" desc="$5" ct="${6:-}" body_file="${7:-}" max_time="${8:-}"
  local code extra=()
  [[ -n "$max_time" ]] && extra+=(--max-time "$max_time")
  # Suppress curl's non-zero exit (e.g. 28 on watch-stream timeout) so we
  # still get the %{http_code} that was received before the body stalled.
  if [[ -n "$body_file" ]]; then
    code="$(curl -s -o /dev/null -w '%{http_code}' "${extra[@]}" -X "$method" -H "Content-Type: $ct" --data-binary "@$body_file" "$BASE$path" 2>/dev/null || true)"
  else
    code="$(curl -s -o /dev/null -w '%{http_code}' "${extra[@]}" -X "$method" "$BASE$path" 2>/dev/null || true)"
  fi
  if [[ "$code" == "$want" ]]; then
    ok "$id  $method $path -> $code  ($desc)"
    PASS=$((PASS+1))
  else
    echo "FAIL $id  $method $path -> $code (expected $want)  ($desc)"
    FAIL=$((FAIL+1))
  fi
}

echo "## golden conformance — empty-cluster baseline + controller loops (port $PORT)"

# --- discovery contract (byte-stable) ---
check_body G01 "/api"                 "$GOLDEN/discovery-api.json"        serveraddr
check_body G02 "/apis"                "$GOLDEN/discovery-apis.json"
check_body G03 "/api/v1"              "$GOLDEN/discovery-core-v1.json"
check_body G04 "/apis/init-pro.io/v1" "$GOLDEN/discovery-initpro-v1.json"

# --- collection endpoints now exist (T1.2b) ---
check_status G05 "/apis/fabricated.io/v9beta1"           "404" "unknown group/version"

source "$SCRIPT_DIR/golden/suites/storage.sh"
source "$SCRIPT_DIR/golden/suites/controllers.sh"
source "$SCRIPT_DIR/golden/suites/scheduler.sh"
source "$SCRIPT_DIR/golden/suites/nodes.sh"
source "$SCRIPT_DIR/golden/suites/argoproj.sh"

echo
echo "golden: $PASS passed, $FAIL failed"
if [[ "$FAIL" -ne 0 ]]; then
  echo "golden conformance FAILED" >&2
  exit 1
fi
echo "ALL golden conformance checks passed"
