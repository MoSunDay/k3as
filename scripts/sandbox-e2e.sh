#!/usr/bin/env bash
# T7.7 (Q63): privileged-Linux e2e for the hardened Agent sandbox — the
# real-machine acceptance of the Q63 containment plane over a real
# `init-pro server` + three `init-pro agent`s (a1: sandbox+broker node with
# credential canaries in its host env; a2: sandbox node with a 5 s exec
# budget; a3: node WITHOUT INIT_PRO_AGENT_SANDBOX), a busybox runner stub
# staged into the private rootfs, and a loopback python3 fake provider.
#
#   SA1  policy determinism: exit 0 -> Succeeded/Completed; exit 7 ->
#        Failed/RunnerFailed "exit 7"; allowTools ["ssh"] -> Failed/
#        PolicyViolation ("policies may only narrow"); a legacy (no
#        spec.sandbox) Agent still runs the Q37 path on a3.
#   SA2  identity + fs/net containment: uid/gid 1000/1000, CapEff 0,
#        NoNewPrivs 1, hostname = agent name, rootfs NOT writable,
#        workspace writable, fresh netns (connect to the LIVE loopback
#        provider fails).
#   SA3  broker lifecycle: socket alive while Running, sandbox env carries
#        the IN-CONTAINER endpoint /broker/provider.sock, revoked at end.
#   SA4  cgroup limits inside the sandbox (268435456 / 64 / "50000
#        100000") + four-way cleanup (natural exit, orphan descendants
#        via cgroup.kill, hang -> ExecTimeout, delete mid-run): no engine
#        process / attempt cgroup / socket after each terminal state.
#   SA5  credential canaries: sandbox env dump, engine log and agent log
#        contain neither the host env key nor the broker credential; the
#        provider log stays EMPTY (no egress).
#   SA6  fail-closed: a hardened Agent on a3 stays Pending/
#        SandboxUnavailable >= 6 s, no engine, never executes.
#
# SKIP doctrine (engine-selftest parity): missing root, static busybox,
# cgroup v2, python3 or curl => one SKIP line, exit 0. Anything else FAILs.
# Usage: scripts/sandbox-e2e.sh  (INIT_PRO_BIN / INIT_PRO_BUSYBOX overrides)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="${INIT_PRO_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/debug/init-pro}"
BUSYBOX="${INIT_PRO_BUSYBOX:-/usr/bin/busybox}"

skip() { echo "SKIP sandbox-e2e: $*"; exit 0; }
[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl >/dev/null 2>&1 || skip "no curl (probes are HTTP-only)"
command -v python3 >/dev/null 2>&1 || skip "no python3 (fake provider)"
command -v pgrep >/dev/null 2>&1 || skip "no pgrep"
[[ "$(id -u)" -eq 0 ]] || skip "requires real root (clone3/mount namespaces)"
[[ -x "$BUSYBOX" ]] || skip "no static busybox at $BUSYBOX"
ldd "$BUSYBOX" >/dev/null 2>&1 && skip "$BUSYBOX is dynamically linked (private rootfs has no libs)"
[[ -f /sys/fs/cgroup/cgroup.controllers ]] || skip "cgroup v2 is not mounted"

pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
    || echo $((20000 + RANDOM % 10000))
}
stop_pid() {  # TERM, up to 5 s grace, then KILL + reap
  local p="$1"
  [[ -n "$p" ]] || return 0
  kill -TERM "$p" 2>/dev/null || true
  for _ in $(seq 1 50); do kill -0 "$p" 2>/dev/null || return 0; sleep 0.1; done
  kill -KILL "$p" 2>/dev/null || true
  wait "$p" 2>/dev/null || true
}

PASS=0; FAIL=0
ok()  { echo "OK    $*"; PASS=$((PASS+1)); }
bad() { echo "FAIL  $*"; FAIL=$((FAIL+1)); }
note(){ echo "----  $*"; }

API_PORT="$(pick_port)"; PROV_PORT="$(pick_port)"
BASE="http://127.0.0.1:$API_PORT"
TMP="$(mktemp -d /tmp/sandbox-e2e.XXXXXX)"
A1="$TMP/a1-data"; A2="$TMP/a2-data"; A3="$TMP/a3-data"
SRV_PID=""; A1_PID=""; A2_PID=""; A3_PID=""; PROV_PID=""

KEEP_TMP="${KEEP_TMP:-0}"
cleanup() {
  trap - EXIT INT TERM
  echo; echo "sandbox e2e: tearing down ..."
  for p in "$A3_PID" "$A2_PID" "$A1_PID" "$PROV_PID" "$SRV_PID"; do stop_pid "$p"; done
  pkill -f "engine serve --socket $TMP/" 2>/dev/null || true  # failed-check strays
  if [[ "$KEEP_TMP" == "1" ]]; then echo "sandbox e2e: logs kept at $TMP (KEEP_TMP=1)"; return; fi
  rm -rf "$TMP" 2>/dev/null || true
  echo "sandbox e2e: bye."
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# --- runner stub staged into the private rootfs (in-container paths) ------------
# stage_runner_into_rootfs copies the runner PARENT DIR to /runner, so the
# static busybox must live next to the stub. The unconditional
# `mkdir -p /workspace/reports` doubles as the execution marker for SA6.
mkdir -p "$TMP/runner"; cp "$BUSYBOX" "$TMP/runner/busybox"; chmod 755 "$TMP/runner/busybox"
cat > "$TMP/runner/stub" <<'STUB'
#!/runner/busybox sh
BB=/runner/busybox
task=""
for a in "$@"; do task="$a"; done
"$BB" mkdir -p /workspace/reports
case "$task" in
  sa1-ok)        echo "[session sa1-ok-s]" >&2; exit 0 ;;
  sa1-fail)      echo "[session sa1-fail-s]" >&2; exit 7 ;;
  sa2-identity)
    R=/workspace/reports/sa2.txt
    "$BB" id -u >"$R"
    "$BB" id -g >>"$R"
    "$BB" hostname >>"$R"
    "$BB" grep -E 'CapEff|NoNewPrivs' /proc/self/status >>"$R"
    if "$BB" touch /rootfs-must-deny 2>/dev/null; then echo ROOTFS-WRITABLE >>"$R"; else echo rootfs-readonly-ok >>"$R"; fi
    echo ws-write-ok >>"$R"
    if "$BB" nc -w 2 127.0.0.1 @@PROV@@ </dev/null >>"$R" 2>&1; then echo NET-EGRESS >>"$R"; else echo netns-isolated-ok >>"$R"; fi
    echo "[session sa2-s]" >&2; exit 0 ;;
  sa3-socket)
    "$BB" env > /workspace/reports/sa3-env.txt
    "$BB" sleep 8
    echo "[session sa3-s]" >&2; exit 0 ;;
  sa4-limits)
    "$BB" cat /sys/fs/cgroup/pids.max /sys/fs/cgroup/memory.max /sys/fs/cgroup/cpu.max > /workspace/reports/sa4-limits.txt
    echo "[session sa4-limits-s]" >&2; exit 0 ;;
  sa4-kids)
    "$BB" sleep 300 & "$BB" sleep 300 & "$BB" sleep 300 &
    echo "[session sa4-kids-s]" >&2; exit 0 ;;
  sa4-hang|sa4-cancel)
    "$BB" sleep 60 ;;
  *)             echo "[session default-s]" >&2; exit 0 ;;
esac
STUB
sed -i "s/@@PROV@@/$PROV_PORT/g" "$TMP/runner/stub"; chmod 755 "$TMP/runner/stub"

# --- legacy Q37 stub (HOST paths; runs uncontained on a3) ------------------------
cat > "$TMP/legacy-stub" <<'LEGACY'
#!/bin/sh
task=""
for a in "$@"; do task="$a"; done
mkdir -p reports
case "$task" in
  *) echo "[session sa1-legacy-s]" >&2; exit 0 ;;
esac
LEGACY
chmod 755 "$TMP/legacy-stub"

# --- fake provider (loopback-only upstream; log must stay EMPTY) -----------------
cat > "$TMP/provider.py" <<'PY'
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
port = int(sys.argv[1]); log = open(sys.argv[2], "a", buffering=1)
class H(BaseHTTPRequestHandler):
    def _note(self):
        log.write("%s %s Authorization=%s\n" % (self.command, self.path, self.headers.get("Authorization", "-")))
    def do_GET(self):
        self._note(); self._send()
    def do_POST(self):
        self._note(); self.rfile.read(int(self.headers.get("Content-Length") or 0)); self._send()
    def _send(self):
        body = b'{"ok":true}'
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a):
        pass
HTTPServer(("127.0.0.1", port), H).serve_forever()
PY
python3 "$TMP/provider.py" "$PROV_PORT" "$TMP/provider.log" &
PROV_PID=$!
for _ in $(seq 1 40); do
  curl -sf -o /dev/null "http://127.0.0.1:$PROV_PORT/" 2>/dev/null && break
  kill -0 "$PROV_PID" 2>/dev/null || break; sleep 0.25
done
curl -sf -o /dev/null "http://127.0.0.1:$PROV_PORT/" || { echo "provider never answered" >&2; exit 1; }
: > "$TMP/provider.log"   # the readiness probe must not count as egress

# --- boot server + agents ---------------------------------------------------------
"$BIN" server --data-dir "$TMP/server-data" --bind-address 127.0.0.1 \
  --https-listen-port "$API_PORT" --disable-controller-manager \
  --disable-scheduler --disable-kube-proxy --disable-anyhome >"$TMP/server.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 60); do
  curl -sf -o /dev/null "$BASE/readyz" 2>/dev/null && break
  curl -sf -o /dev/null "$BASE/api/v1/namespaces/default/configmaps" 2>/dev/null && break
  kill -0 "$SRV_PID" 2>/dev/null || break; sleep 0.5
done
curl -sf -o /dev/null "$BASE/api/v1/namespaces/default/configmaps" \
  || { echo "--- server log ---"; cat "$TMP/server.log"; echo "server never answered on $BASE" >&2; exit 1; }
note "S0  server up on $BASE, fake provider on 127.0.0.1:$PROV_PORT"

# start_agent DD LOG NODE KUBELET_PORT [ENV=V ...]; PID lands in $!.
start_agent() {
  local dd="$1" logf="$2" node="$3" kp="$4"; shift 4
  env "$@" "$BIN" agent --data-dir "$dd" --server "$BASE" --token e2e-token \
    --node-name "$node" --kubelet-port "$kp" --flannel-backend none \
    --anyhome-addr= >"$logf" 2>&1 &
}
# a1: hardened sandbox + per-attempt broker; host env has canaries it must NEVER see.
start_agent "$A1" "$TMP/a1.log" sbx-a1 10273 \
  INIT_PRO_AGENT_SANDBOX=1 \
  INIT_PRO_AGENT_RUNNER_BIN="$TMP/runner/stub" \
  INIT_PRO_AGENT_BROKER_UPSTREAM="http://127.0.0.1:$PROV_PORT" \
  INIT_PRO_AGENT_BROKER_OPS="chat, embed" \
  INIT_PRO_AGENT_BROKER_CREDENTIAL="secret-canary-sbx" \
  OPENROUTER_API_KEY="env-canary-zz9"
A1_PID=$!
# a2: hardened sandbox, 5 s exec budget (SA4c), no broker.
start_agent "$A2" "$TMP/a2.log" sbx-a2 10274 \
  INIT_PRO_AGENT_SANDBOX=1 \
  INIT_PRO_AGENT_RUNNER_BIN="$TMP/runner/stub" \
  INIT_PRO_AGENT_EXEC_TIMEOUT_SECS=5
A2_PID=$!
# a3: NO sandbox plane (SA6 fail-closed + SA1d Q37 parity).
start_agent "$A3" "$TMP/a3.log" sbx-a3 10275 \
  INIT_PRO_AGENT_RUNNER_BIN="$TMP/legacy-stub"
A3_PID=$!
sleep 2
note "S1  agents booted (a1 sandbox+broker, a2 sandbox 5s-budget, a3 plain)"

AGENTS="/apis/init-pro.io/v1/namespaces/default/agents"
SANDBOX=',"sandbox":{"mode":"hardened","limits":{"cpuMillis":500,"memoryBytes":268435456,"pidsMax":64}}'

# post NAME NODE PROMPT [SPEC_FRAGMENT] — "" (explicit) = legacy Q37, no
# spec.sandbox; omitting the arg = the standard hardened sandbox fragment.
# Fragments carry their own leading comma (they splice after "nodeName").
post() {
  local name="$1" node="$2" prompt="$3" frag="${4-$SANDBOX}" body
  body='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"NAME","namespace":"default"},"spec":{"prompt":"PROMPT","nodeName":"NODE"FRAG},"status":{"phase":"Pending"}}'
  body="${body/NAME/$name}"; body="${body/PROMPT/$prompt}"; body="${body/NODE/$node}"; body="${body/FRAG/$frag}"
  curl -s -o "$TMP/post-$name.out" -w '%{http_code}' -X POST \
    -H 'Content-Type: application/json' --data-binary "$body" "$BASE$AGENTS"
}

# poll NAME SECONDS PHASE [REASON]: 0 iff reached in budget; last body kept
# at $TMP/obs-$name.json for substring assertions.
poll() {
  local name="$1" budget="$2" phase="$3" reason="${4:-}" url raw=""
  url="$BASE$AGENTS/$name"
  for _ in $(seq 1 $((budget * 2))); do
    raw="$(curl -s "$url" || true)"
    if echo "$raw" | grep -q "\"phase\":\"$phase\"" \
       && { [[ -z "$reason" ]] || echo "$raw" | grep -q "\"reason\":\"$reason\""; }; then
      printf '%s' "$raw" > "$TMP/obs-$name.json"; return 0
    fi
    sleep 0.5
  done
  printf '%s' "$raw" > "$TMP/obs-$name.json"; return 1
}

# no_leftovers LABEL DATADIR NAME: after 2 s teardown grace the attempt name
# prefix has NO engine process, cgroup dir, or sockets (four-way AS4 face).
no_leftovers() {
  local label="$1" dd="$2" name="$3"
  sleep 2
  if pgrep -f "engine serve --socket $dd/run/engine/$name-" >/dev/null 2>&1; then
    bad "$label engine process still alive for $name"; return 1
  fi
  if ls -d "/sys/fs/cgroup/init-pro/$name-"* >/dev/null 2>&1; then
    bad "$label cgroup dir leaked for $name"; return 1
  fi
  if ls "$dd/run/engine/$name-"*.sock >/dev/null 2>&1 \
     || ls "$dd/run/agent-broker/$name-"*.sock >/dev/null 2>&1; then
    bad "$label socket leaked for $name"; return 1
  fi
  ok "$label no engine process / cgroup / sockets left for $name"
}

# --- SA1: policy determinism -------------------------------------------------------
CODE="$(post sa1-ok sbx-a1 sa1-ok || echo 000)"
if poll sa1-ok 40 Succeeded Completed; then
  ok "SA1a exit 0 -> Succeeded/Completed"
else
  bad "SA1a no Succeeded/Completed (POST $CODE; $(tail -2 "$TMP/a1.log" 2>/dev/null | tr '\n' ' '))"
fi
CODE="$(post sa1-fail sbx-a1 sa1-fail || echo 000)"
if poll sa1-fail 40 Failed RunnerFailed && grep -q "exit 7" "$TMP/obs-sa1-fail.json"; then
  ok "SA1b exit 7 -> Failed/RunnerFailed (exit 7 in message)"
else
  bad "SA1b no RunnerFailed/exit 7 (POST $CODE)"
fi
CODE="$(post sa1-widen sbx-a1 sa1-ok ",\"policy\":{\"allowTools\":[\"ssh\"]}$SANDBOX" || echo 000)"
if poll sa1-widen 40 Failed PolicyViolation \
   && grep -q "policies may only narrow" "$TMP/obs-sa1-widen.json"; then
  ok "SA1c allowTools [ssh] widens the baseline -> PolicyViolation (deterministic deny)"
else
  bad "SA1c widening not denied as PolicyViolation (POST $CODE)"
fi
CODE="$(post sa1-legacy sbx-a3 sa1-legacy "" || echo 000)"
if poll sa1-legacy 40 Succeeded Completed; then
  ok "SA1d legacy (no spec.sandbox) Agent still runs the Q37 path on a3"
else
  bad "SA1d legacy path broken (POST $CODE)"
fi

# --- SA2: identity + fs/net containment ---------------------------------------------
CODE="$(post sa2-identity sbx-a1 sa2-identity || echo 000)"
R="$A1/agent-ws/sa2-identity/reports/sa2.txt"
if poll sa2-identity 40 Succeeded Completed && [[ -f "$R" ]] \
   && [[ "$(sed -n 1p "$R")" == "1000" && "$(sed -n 2p "$R")" == "1000" ]] \
   && grep -q "CapEff:[[:space:]]*0000000000000000" "$R" \
   && grep -q "NoNewPrivs:[[:space:]]*1" "$R" \
   && grep -qx "sa2-identity" "$R" \
   && grep -q "rootfs-readonly-ok" "$R" && grep -q "ws-write-ok" "$R" \
   && grep -q "netns-isolated-ok" "$R" && ! grep -q "NET-EGRESS" "$R"; then
  ok "SA2  uid/gid 1000/1000, CapEff 0, NoNewPrivs 1, hostname, ro rootfs, rw workspace, net-isolated"
else
  bad "SA2  containment observations wrong (POST $CODE): $(tr '\n' '|' < "$R" 2>/dev/null)"
fi

# --- SA3: per-attempt broker lifecycle -----------------------------------------------
CODE="$(post sa3-socket sbx-a1 sa3-socket || echo 000)"
SOCK=""; LIVE_OK=0
if poll sa3-socket 40 Running; then
  SOCK="$(ls "$A1/run/agent-broker/sa3-socket-"*.sock 2>/dev/null | head -1 || true)"
  [[ -S "$SOCK" ]] && LIVE_OK=1
fi
if poll sa3-socket 40 Succeeded Completed; then
  if [[ "$LIVE_OK" != "1" ]]; then
    bad "SA3  broker socket not observed while Running (POST $CODE)"
  elif [[ -e "$SOCK" ]]; then
    bad "SA3  broker socket NOT revoked after the attempt ($SOCK)"
  elif grep -q "^INIT_PRO_AGENT_BROKER=/broker/provider.sock$" "$A1/agent-ws/sa3-socket/reports/sa3-env.txt"; then
    ok "SA3  broker socket live during Running, env=/broker/provider.sock, revoked at end"
  else
    bad "SA3  env lacks the in-container broker endpoint: $(tr '\n' '|' < "$A1/agent-ws/sa3-socket/reports/sa3-env.txt" 2>/dev/null)"
  fi
  no_leftovers SA3 "$A1" sa3-socket
else
  bad "SA3  sa3-socket never Completed (POST $CODE)"
fi

# --- SA4: cgroup limits + four-way cleanup ---------------------------------------------
CODE="$(post sa4-limits sbx-a1 sa4-limits || echo 000)"
L="$A1/agent-ws/sa4-limits/reports/sa4-limits.txt"
if poll sa4-limits 40 Succeeded Completed \
   && grep -qx 64 "$L" && grep -qx 268435456 "$L" && grep -qx "50000 100000" "$L"; then
  ok "SA4a sandbox cgroup limits: pids 64, memory 268435456, cpu 50000 100000"
else
  bad "SA4a limits wrong (POST $CODE): $(tr '\n' '|' < "$L" 2>/dev/null)"
fi
CODE="$(post sa4-kids sbx-a2 sa4-kids || echo 000)"
if poll sa4-kids 40 Succeeded Completed; then
  sleep 2
  if pgrep -f "/runner/busybox sleep 300" >/dev/null 2>&1; then
    bad "SA4b orphan descendants survived the natural exit (cgroup.kill gap)"
  else
    ok "SA4b orphan sleep descendants reaped by the attempt teardown"
  fi
  no_leftovers SA4b "$A2" sa4-kids
else
  bad "SA4b run missing (POST $CODE)"
fi
CODE="$(post sa4-hang sbx-a2 sa4-hang || echo 000)"
if poll sa4-hang 40 Failed ExecTimeout; then
  ok "SA4c hung runner (60 s) killed at the 5 s budget -> Failed/ExecTimeout"
  no_leftovers SA4c "$A2" sa4-hang
else
  bad "SA4c no ExecTimeout (POST $CODE)"
fi
CODE="$(post sa4-cancel sbx-a1 sa4-cancel || echo 000)"
if poll sa4-cancel 40 Running; then
  curl -s -o /dev/null -X DELETE "$BASE$AGENTS/sa4-cancel"
  GONE=0
  for _ in $(seq 1 40); do
    curl -sf -o /dev/null "$BASE$AGENTS/sa4-cancel" 2>/dev/null || { GONE=1; break; }
    sleep 0.5
  done
  if [[ "$GONE" == "1" ]]; then
    ok "SA4d delete mid-run removed the object"
    no_leftovers SA4d "$A1" sa4-cancel
  else
    bad "SA4d object survived DELETE"
  fi
else
  bad "SA4d never reached Running (POST $CODE)"
fi

# --- SA5: credential canaries never reach the sandbox or logs ---------------------------
S5=0
for f in "$A1/agent-ws/sa3-socket/reports/sa3-env.txt" "$A1/agent-ws/sa2-identity/reports/sa2.txt"; do
  grep -q "canary" "$f" 2>/dev/null && { bad "SA5  canary leaked into sandbox env dump ($f)"; S5=1; }
done
grep -rq "canary" "$A1/run/engine/" 2>/dev/null && { bad "SA5  canary leaked into an engine log"; S5=1; }
grep -q "canary" "$TMP/a1.log" 2>/dev/null && { bad "SA5  canary leaked into the agent log"; S5=1; }
[[ -s "$TMP/provider.log" ]] && { bad "SA5  provider saw egress: $(cat "$TMP/provider.log")"; S5=1; }
[[ "$S5" == "0" ]] && ok "SA5  no env key / broker credential in sandbox env, engine log, agent log; provider log empty"

# --- SA6: fail-closed on the node without the sandbox plane -----------------------------
CODE="$(post sa6-failclosed sbx-a3 sa1-ok || echo 000)"
if poll sa6-failclosed 15 Pending SandboxUnavailable; then
  sleep 6
  if poll sa6-failclosed 2 Pending SandboxUnavailable \
     && ! pgrep -f "engine serve --socket $A3/run/engine/sa6-failclosed-" >/dev/null 2>&1 \
     && ! ls -d "$A3/agent-ws/sa6-failclosed/reports" >/dev/null 2>&1; then
    ok "SA6  hardened Agent on a plain node stays Pending/SandboxUnavailable >= 6 s, no engine, never executed"
  else
    bad "SA6  fail-closed broke: $(grep -o '"reason":"[^"]*"' "$TMP/obs-sa6-failclosed.json" | head -1)"
  fi
else
  bad "SA6  no Pending/SandboxUnavailable (POST $CODE)"
fi

# --- summary -------------------------------------------------------------------
echo; echo "sandbox e2e: $PASS passed, $FAIL failed"
if [[ "$FAIL" == "0" ]]; then echo "ALL sandbox e2e checks passed (SA1-SA6, T7.7/Q63)"; exit 0; fi
echo "sandbox e2e FAILED" >&2; exit 1
