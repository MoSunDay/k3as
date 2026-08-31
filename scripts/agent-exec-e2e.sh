#!/usr/bin/env bash
# T7.1 Phase B (Q37): real-machine e2e probe over the node-side Agent
# execution loop. Boots `init-pro server` + several `init-pro agent`s and
# asserts, over the REAL apiserver HTTP surface:
#
#   G1  REQUIRED: a bound Agent (spec.nodeName set) is executed by the
#       agent's opencoder stub: Pending -> Running -> Succeeded, condition
#       Ready=True reason=Completed, message carries the `[session e2e-ok-s]`
#       marker the stub prints (merge-patch write-back proof).
#   G2  REQUIRED: the run WORKDIR is the workspace (Q36/Q37 contract): the
#       stub drops `marker.txt` into its cwd; in anyhome mode it MUST land
#       in <server-data>/anyhome/fs/agent/e2e-g1/, in local-fallback mode in
#       <agent-data>/agent-ws/e2e-g1/.
#   G3  REQUIRED: stub exit 7 -> phase Failed, reason RunnerFailed, message
#       carries "exit 7".
#   G4  REQUIRED: hung stub (sleep 60) -> phase Failed reason ExecTimeout
#       (agent booted with INIT_PRO_AGENT_EXEC_TIMEOUT_SECS=3).
#   G5  REQUIRED: agent WITHOUT a runner binary (INIT_PRO_AGENT_RUNNER_BIN
#       pointing at a missing path => discover returns None, no fallback)
#       keeps its Agent Pending with Ready=False reason=RunnerMissing.
#   G6  REQUIRED (anyhome mode): the server-side keeper scaffolded the
#       shared workspace bytes (skills/ tools/ mem/ + prompt.md) under
#       <server-data>/anyhome/fs/agent/e2e-g1/.
#   G7  INFORMATIONAL: `--disable-agent-exec` keeps bound Agents Pending.
#
# SKIP doctrine (mirrors anyhome-e2e.sh): no curl -> SKIP the whole run
# (the probes are HTTP-only). Missing root/mount.nfs does NOT skip: the run
# degrades to the node-local workspace fallback (agent2 gets
# `--anyhome-addr=""`, annotation-only mode) and G6 is dropped.
#
# Usage: scripts/agent-exec-e2e.sh
#   INIT_PRO_BIN=...  reuse a specific binary (default target/debug/init-pro)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl >/dev/null 2>&1 || { echo "SKIP agent-exec-e2e: no curl (probes are HTTP-only)"; exit 0; }

skip() { echo "SKIP agent-exec-e2e: $*"; exit 0; }

# --- preflight ---------------------------------------------------------------
MOUNT_NFS="$(command -v mount.nfs 2>/dev/null || true)"
for c in /sbin/mount.nfs /usr/sbin/mount.nfs /usr/local/sbin/mount.nfs; do
  [[ -n "$MOUNT_NFS" ]] && break
  [[ -x "$c" ]] && MOUNT_NFS="$c"
done
IS_ROOT=0; [[ "$(id -u)" == "0" ]] && IS_ROOT=1
NFS_MODE=0
if [[ "$IS_ROOT" == "1" && -n "$MOUNT_NFS" ]]; then NFS_MODE=1; fi

pick_port() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
      || echo $((20000 + RANDOM % 10000))
  else
    echo $((20000 + RANDOM % 10000))
  fi
}

stop_pid() {
  local p="$1" i
  [[ -n "$p" ]] || return 0
  kill -TERM "$p" 2>/dev/null || true
  for i in $(seq 1 50); do
    kill -0 "$p" 2>/dev/null || return 0
    sleep 0.1
  done
  kill -KILL "$p" 2>/dev/null || true
  wait "$p" 2>/dev/null || true
}

PASS=0; FAIL=0; WARN=0
ok()   { echo "OK    $*"; PASS=$((PASS+1)); }
bad()  { echo "FAIL  $*"; FAIL=$((FAIL+1)); }
warn() { echo "WARN  $*"; WARN=$((WARN+1)); }

PORT="$(pick_port)"        # anyhome NFS plane
API_PORT="$(pick_port)"    # apiserver
BASE="http://127.0.0.1:$API_PORT"
TMP="$(mktemp -d /tmp/agent-exec-e2e.XXXXXX)"
DATA="$TMP/server-data"
SRV_LOG="$TMP/server.log"
AG1_LOG="$TMP/agent1.log"; AG2_LOG="$TMP/agent2.log"; AG3_LOG="$TMP/agent3.log"
SRV_PID=""; AG1_PID=""; AG2_PID=""; AG3_PID=""

# The behavior-switching opencoder stub: the LAST positional arg is the
# task prompt (spec.prompt), so one binary serves every probe.
STUB="$TMP/opencoder"
cat > "$STUB" <<'STUBEOF'
#!/bin/sh
export PATH=/bin:/usr/bin
task=""
for a in "$@"; do task="$a"; done
case "$task" in
  e2e-fail)   echo "[session e2e-fail-s]" >&2; exit 7 ;;
  e2e-hang)   sleep 60 ;;
  e2e-marker) echo "marker $task" > marker.txt; echo "[session e2e-ok-s]" >&2; exit 0 ;;
  *)          echo "[session e2e-ok-s]" >&2; exit 0 ;;
esac
STUBEOF
chmod 755 "$STUB"

KEEP_TMP="${KEEP_TMP:-0}"
cleanup() {
  trap - EXIT INT TERM
  echo
  echo "agent-exec e2e: tearing down ..."
  for p in "$AG3_PID" "$AG2_PID" "$AG1_PID" "$SRV_PID"; do stop_pid "$p"; done
  # The per-address workspace root mounts stay for the loop's lifetime by
  # design; the run is over, so unmount them before the datadir rm (an
  # EBUSY here would also leak system-wide mounts into other gates).
  while read -r _ mp _; do
    case "$mp" in "$TMP"/*) umount -f "$mp" 2>/dev/null || umount -l "$mp" 2>/dev/null || true ;; esac
  done < /proc/self/mounts
  if [[ "$KEEP_TMP" == "1" ]]; then
    echo "agent-exec e2e: logs kept at $TMP (KEEP_TMP=1)"
    return
  fi
  rm -rf "$TMP" 2>/dev/null || true
  echo "agent-exec e2e: bye."
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

post_agent() { # name node prompt [workspace-annotation]
  local name="$1" node="$2" prompt="$3" ann="${4:-}" body annjson=""
  if [[ -n "$ann" ]]; then
    annjson=",\"annotations\":{\"init-pro.io/workspace\":\"$ann\"}"
  fi
  body="{\"apiVersion\":\"init-pro.io/v1\",\"kind\":\"Agent\",\"metadata\":{\"name\":\"$name\",\"namespace\":\"default\"$annjson},\"spec\":{\"prompt\":\"$prompt\",\"nodeName\":\"$node\"},\"status\":{\"phase\":\"Pending\"}}"
  curl -s -o "$TMP/post-$name.out" -w '%{http_code}' -X POST \
    -H 'Content-Type: application/json' --data-binary "$body" \
    "$BASE/apis/init-pro.io/v1/namespaces/default/agents"
}

# poll_agent NAME SECONDS PHASE [REASON]: 0 iff the object reaches PHASE
# (and, when given, a Ready condition with that reason) within SECONDS.
poll_agent() {
  local name="$1" budget="$2" phase="$3" reason="${4:-}" url raw i
  url="$BASE/apis/init-pro.io/v1/namespaces/default/agents/$name"
  for i in $(seq 1 $((budget * 2))); do
    raw="$(curl -s "$url" || true)"
    if echo "$raw" | grep -q "\"phase\":\"$phase\""; then
      if [[ -z "$reason" ]] || echo "$raw" | grep -q "\"reason\":\"$reason\""; then
        printf '%s' "$raw" > "$TMP/obs-$name.json"
        return 0
      fi
    fi
    sleep 0.5
  done
  printf '%s' "$raw" > "$TMP/obs-$name.json"
  return 1
}

# --- boot server + agents ----------------------------------------------------
# Server: anyhome on $PORT; controllers/scheduler/kube-proxy off (the probes
# use PRE-BOUND agents, so no scheduler is needed to fill spec.nodeName).
"$BIN" server \
  --data-dir "$DATA" \
  --bind-address 127.0.0.1 \
  --https-listen-port "$API_PORT" \
  --anyhome-addr "127.0.0.1:$PORT" \
  --disable-controller-manager \
  --disable-scheduler \
  --disable-kube-proxy >"$SRV_LOG" 2>&1 &
SRV_PID=$!

for _ in $(seq 1 60); do
  curl -sf -o /dev/null "$BASE/readyz" 2>/dev/null && break
  curl -sf -o /dev/null "$BASE/api/v1/namespaces/default/configmaps" 2>/dev/null && break
  kill -0 "$SRV_PID" 2>/dev/null || break
  sleep 0.5
done
if ! curl -sf -o /dev/null "$BASE/api/v1/namespaces/default/configmaps"; then
  echo "--- server log ($SRV_LOG) ---"; cat "$SRV_LOG" || true
  bad "server never answered on $BASE"
  echo "agent-exec e2e: $PASS passed, $FAIL failed, $WARN warnings"
  exit 1
fi
ok "S1  server up on $BASE (anyhome on 127.0.0.1:$PORT)"

# Workspace mode for the EXECUTOR agent: NFS subpath mounts when root +
# mount.nfs exist (the real Q36/Q37 flow), node-local fallback otherwise.
if [[ "$NFS_MODE" == "1" ]]; then
  AG2_ADDR="127.0.0.1:$PORT"
else
  AG2_ADDR=""   # empty --anyhome-addr = annotation-only mode (local fallback)
fi

# agent2: the executor. 3s exec budget so G4 (hang -> ExecTimeout) is quick.
AG2_FLAGS=(--data-dir "$TMP/agent2-data" --server "$BASE" --token e2e-token --node-name e2e-agent2 --kubelet-port 10261 --flannel-backend none)
if [[ -n "$AG2_ADDR" ]]; then
  AG2_FLAGS+=(--anyhome-addr "$AG2_ADDR")
else
  AG2_FLAGS+=(--anyhome-addr="")   # empty = annotation-only (local fallback)
fi
INIT_PRO_AGENT_RUNNER_BIN="$STUB" INIT_PRO_AGENT_EXEC_TIMEOUT_SECS=3 \
  "$BIN" agent "${AG2_FLAGS[@]}" >"$AG2_LOG" 2>&1 &
AG2_PID=$!

# agent3: NO runner binary (override points at a missing path; discovery
# treats an explicit-but-missing override as RunnerMissing, no fallback).
INIT_PRO_AGENT_RUNNER_BIN="$TMP/definitely-not-here" \
  "$BIN" agent \
    --data-dir "$TMP/agent3-data" \
    --server "$BASE" \
    --token e2e-token \
    --node-name e2e-agent3 --kubelet-port 10262 \
    --flannel-backend none \
    --anyhome-addr="" >"$AG3_LOG" 2>&1 &
AG3_PID=$!

# agent1: the loop is OFF entirely (--disable-agent-exec); bound Agents
# must stay Pending forever (G7).
"$BIN" agent \
    --data-dir "$TMP/agent1-data" \
    --server "$BASE" \
    --token e2e-token \
    --node-name e2e-agent1 --kubelet-port 10263 \
    --flannel-backend none \
    --disable-agent-exec >"$AG1_LOG" 2>&1 &
AG1_PID=$!

sleep 1
ok "S2  agents booted (executor mode: $( [[ $NFS_MODE == 1 ]] && echo anyhome-nfs || echo local-fallback ))"

# --- G1: happy path (REQUIRED) ----------------------------------------------
CODE="$(post_agent e2e-g1 e2e-agent2 e2e-marker || echo 000)"
if poll_agent e2e-g1 30 Succeeded Completed \
   && grep -q "session e2e-ok-s" "$TMP/obs-e2e-g1.json"; then
  ok "G1  agent executed to Succeeded (Ready=True Completed, session marker in status.message)"
else
  bad "G1  agent did not reach Succeeded/Completed (POST code: $CODE; see $AG2_LOG)"
fi

# --- G2: workdir is the workspace (REQUIRED) ---------------------------------
if [[ "$NFS_MODE" == "1" ]]; then
  WS_MARKER="$DATA/anyhome/fs/agent/e2e-g1/marker.txt"
else
  WS_MARKER="$TMP/agent2-data/agent-ws/e2e-g1/marker.txt"
fi
if grep -q "marker e2e-marker" "$WS_MARKER" 2>/dev/null; then
  ok "G2  stub cwd was the workspace ($WS_MARKER)"
else
  bad "G2  marker.txt missing at $WS_MARKER"
fi

# --- G3: nonzero exit -> RunnerFailed (REQUIRED) ------------------------------
CODE="$(post_agent e2e-g3 e2e-agent2 e2e-fail || echo 000)"
if poll_agent e2e-g3 30 Failed RunnerFailed \
   && grep -q "exit 7" "$TMP/obs-e2e-g3.json"; then
  ok "G3  nonzero exit surfaced as Failed/RunnerFailed (exit 7 in message)"
else
  bad "G3  agent did not surface RunnerFailed (POST code: $CODE)"
fi

# --- G4: hang -> ExecTimeout (REQUIRED) --------------------------------------
CODE="$(post_agent e2e-g4 e2e-agent2 e2e-hang || echo 000)"
if poll_agent e2e-g4 30 Failed ExecTimeout; then
  ok "G4  hung runner killed and marked Failed/ExecTimeout (3s budget)"
else
  bad "G4  hung agent did not surface ExecTimeout (POST code: $CODE)"
fi

# --- G5: no runner binary -> RunnerMissing, stays Pending (REQUIRED) ----------
CODE="$(post_agent e2e-g5 e2e-agent3 noprompt || echo 000)"
if poll_agent e2e-g5 30 Pending RunnerMissing; then
  ok "G5  runnerless agent stays Pending with Ready=False RunnerMissing"
else
  bad "G5  runnerless agent did not report RunnerMissing (POST code: $CODE)"
fi

# --- G6: keeper scaffold of the shared workspace bytes -----------------------
if [[ "$NFS_MODE" == "1" ]]; then
  WS_DIR="$DATA/anyhome/fs/agent/e2e-g1"
  if [[ -d "$WS_DIR/skills" && -d "$WS_DIR/tools" && -d "$WS_DIR/mem" && -f "$WS_DIR/prompt.md" ]]; then
    ok "G6  keeper scaffolded skills/ tools/ mem/ prompt.md in the anyhome export"
  else
    bad "G6  keeper scaffold incomplete under $WS_DIR"
  fi
else
  WS_DIR="$TMP/agent2-data/agent-ws/e2e-g1"
  if [[ -d "$WS_DIR/skills" && -d "$WS_DIR/mem" && -L "$WS_DIR/.opencoder/skills" ]]; then
    ok "G6  node-local scaffold present (skills/ mem/ + .opencoder/skills bridge)"
  else
    warn "G6  node-local scaffold incomplete under $WS_DIR (informational)"
  fi
fi

# --- G7: --disable-agent-exec keeps Agents Pending (INFORMATIONAL) ------------
CODE="$(post_agent e2e-g7 e2e-agent1 "" || echo 000)"
if poll_agent e2e-g7 6 Pending && ! grep -q '"phase":"Running"' "$TMP/obs-e2e-g7.json"; then
  ok "G7  --disable-agent-exec kept the bound Agent Pending (informational)"
else
  warn "G7  disabled-loop agent changed phase (informational, POST code: $CODE)"
fi

# --- summary ------------------------------------------------------------------
echo
echo "agent-exec e2e: $PASS passed, $FAIL failed, $WARN warnings"
if [[ "$FAIL" == "0" ]]; then
  echo "ALL agent-exec e2e required checks passed (G1-G5$( [[ $NFS_MODE == 1 ]] && echo +G6 ))"
  exit 0
fi
echo "agent-exec e2e FAILED" >&2
exit 1
