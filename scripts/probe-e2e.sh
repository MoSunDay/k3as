#!/usr/bin/env bash
# T4.8: tcpSocket + exec probe e2e. Boots a real single-node init-pro
# cluster (server + agent over the bundled containerd, T4.1/T4.2) and
# drives all four probe outcome quadrants through the echo image
# (`scripts/build-echo-image.sh`: `/bin/echo` HTTP server on :8080 +
# `/bin/wfstep`, the Q27 one-shot -- exit 0 / --fail exit 1):
#
#   P1  readinessProbe tcpSocket :8080 (open)   -> pod Running + Ready=True
#   P2  livenessProbe  tcpSocket :9    (closed) -> restartCount >= 1
#   P3  readinessProbe exec ["wfstep","ok"]     -> Ready=True (rc==0 healthy)
#   P4  livenessProbe  exec ["wfstep","--fail"] -> restartCount >= 1 (rc!=0)
#
# Probe knobs are tightened (delay/period/timeout/thresholds 1s/1) so the
# quadrants resolve in seconds; restart assertions poll restartCount
# NUMERICALLY (>= 1) -- an exact count is flaky by design (the probe keeps
# failing after each restart, by design).
#
# SKIP semantics (Q25 doctrine, G24/G25 parity): without a vendored
# containerd bundle or `cc` for the offline image builder, the suite
# prints a SKIP line and exits 0 -- missing build inputs are a
# configuration state, not a conformance break. Requires a PRE-BUILT
# binary (cargo build), curl, python3, cc, vendored containerd.
#
# Usage: scripts/probe-e2e.sh
#   INIT_PRO_BIN=...  reuse a specific binary (default target/debug/init-pro)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl    >/dev/null || { echo "error: curl is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }

skip() { echo "SKIP probe-e2e: $*"; exit 0; }

# Vendor detection mirrors runtime::stage::vendor_bin_root() and golden G24.
VENDOR_BIN=""
if [[ -n "${INIT_PRO_VENDOR_BIN:-}" && -f "$INIT_PRO_VENDOR_BIN/containerd" ]]; then
  VENDOR_BIN="$INIT_PRO_VENDOR_BIN"
else
  for c in "$(dirname "$BIN")/../../vendor/bin" "$(dirname "$BIN")/../vendor/bin" "$PWD/vendor/bin" "$ROOT/vendor/bin"; do
    [[ -f "$c/containerd" ]] && { VENDOR_BIN="$c"; break; }
  done
fi
[[ -n "$VENDOR_BIN" ]] || skip "no vendored containerd bundle (build with INIT_PRO_VENDOR=1)"
CC="${CC:-cc}"
command -v "$CC" >/dev/null 2>&1 || skip "no C compiler for the offline echo image"

pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
    || echo $((20000 + RANDOM % 10000))
}
stop_pid() {
  local pid="$1" i
  kill -TERM "$pid" 2>/dev/null || { wait "$pid" 2>/dev/null || true; return 0; }
  for i in $(seq 1 40); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.25
  done
  kill -KILL "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

PASS=0; FAIL=0
ok()  { echo "OK   $*"; PASS=$((PASS+1)); }
bad() { echo "FAIL $*"; FAIL=$((FAIL+1)); }

API_PORT="$(pick_port)"
BASE="http://127.0.0.1:${API_PORT}"
DD="$(mktemp -d)"
SRV_LOG="$DD/server.log"; AG_LOG="$DD/agent.log"
SRV_PID=""; AGENT_PID=""
PAUSE_REF="init-pro.local/pause:0.1"
ECHO_REF="init-pro.local/echo:0.1"
NODE_NAME="probe-node"

# CNI device pre-existence flags, S40c followup: probed BEFORE the trap is
# installed so the cleanup below only ever deletes devices THIS run created;
# a pre-existing cni0/flannel.1 (host / live cluster we coexist with) is
# left untouched.
CNI0_PREEXISTS=0
ip link show dev cni0 >/dev/null 2>&1 && CNI0_PREEXISTS=1
FLANNEL1_PREEXISTS=0
ip link show dev flannel.1 >/dev/null 2>&1 && FLANNEL1_PREEXISTS=1

cleanup() {
  trap - EXIT INT TERM HUP
  [[ -n "$AGENT_PID" ]] && stop_pid "$AGENT_PID"
  [[ -n "$SRV_PID" ]]   && stop_pid "$SRV_PID"
  sleep 0.5
  for p in $(pgrep -f "$DD/.*containerd-shim" 2>/dev/null || true); do
    kill -KILL "$p" 2>/dev/null || true
  done
  for m in $(mount | awk -v dd="$DD/" 'index($3, dd) == 1 {print $3}'); do
    umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
  done
  rm -rf "$DD" 2>/dev/null || true
  # CNI hygiene (S40c followup): remove devices THIS run created; a
  # pre-existing cni0/flannel.1 (host/live-cluster) is left untouched.
  if [[ "$CNI0_PREEXISTS" != "1" ]]; then ip link del cni0 2>/dev/null || true; fi
  if [[ "$FLANNEL1_PREEXISTS" != "1" ]]; then ip link del flannel.1 2>/dev/null || true; fi
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

echo "probe e2e: building the offline images (pause + echo 8080/wfstep) ..."
"$SCRIPT_DIR/build-pause-image.sh" "$DD/pause.tar" "$PAUSE_REF" >/dev/null
"$SCRIPT_DIR/build-echo-image.sh" "$DD/echo.tar" "$ECHO_REF" >/dev/null

# --- cluster boot (pod-env-e2e coexistence recipe) -----------------------------
# Cluster CIDR: reuse the host cni0 /16 when a live init-pro agent already
# programs the shared bridge (the vendored bridge plugin errors on a
# mismatched pre-existing cni0); default 10.42.0.0/16 on a clean host.
CLUSTER_CIDR="10.42.0.0/16"
CNI0_ADDR="$(ip -4 -o addr show dev cni0 2>/dev/null | awk '{print $4; exit}')"
if [[ "$CNI0_ADDR" =~ ^([0-9]+\.[0-9]+)\.[0-9]+\.[0-9]+/ ]]; then
  CLUSTER_CIDR="${BASH_REMATCH[1]}.0.0/16"
  echo "probe e2e: host cni0 is ${CNI0_ADDR}; reusing ${CLUSTER_CIDR}"
fi

echo "probe e2e: booting cluster (API $BASE, datadir $DD, cidr $CLUSTER_CIDR)"
"$BIN" server --data-dir "$DD/srv" --bind-address 127.0.0.1 --https-listen-port "$API_PORT" \
  --cluster-cidr "$CLUSTER_CIDR" >"$SRV_LOG" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 60); do grep -q "discovery listening" "$SRV_LOG" && break; sleep 0.1; done
grep -q "discovery listening" "$SRV_LOG" || { echo "error: server never reported listening"; cat "$SRV_LOG" >&2; exit 1; }

KUBELET_PORT="$(pick_port)"
INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" "$BIN" agent --data-dir "$DD/agent" --token dev \
    --server "$BASE" --node-name "$NODE_NAME" --cluster-cidr "$CLUSTER_CIDR" \
    --node-ip 127.0.0.1 --kubelet-port "$KUBELET_PORT" >"$AG_LOG" 2>&1 &
AGENT_PID=$!
for _ in $(seq 1 300); do grep -q "containerd healthy" "$AG_LOG" && break; sleep 0.1; done
grep -q "containerd healthy" "$AG_LOG" || { echo "error: containerd never healthy"; cat "$AG_LOG" >&2; exit 1; }
INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$DD/pause.tar" >/dev/null
INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$DD/echo.tar" >/dev/null

# --- helpers -------------------------------------------------------------------
post_json() {
  curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary @"$2" "$BASE$1"
}
new_pod() { # <name> <file> <container-json> -> POSTs the pod or dies
  printf '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"%s","namespace":"default"},"spec":{"nodeName":"%s","containers":[%s]}}' \
    "$1" "$NODE_NAME" "$3" > "$DD/$2"
  CODE="$(post_json "/api/v1/namespaces/default/pods" "$DD/$2")"
  [[ "$CODE" == "201" ]] || { echo "error: pod $1 POST -> $CODE" >&2; exit 1; }
}
pod_field() { # <pod> <python-expr over p> -> prints it or ""
  curl -s "$BASE/api/v1/namespaces/default/pods/$1" | python3 -c '
import json,sys
try: p=json.load(sys.stdin)
except Exception: raise SystemExit(0)
print('"$2"' or "")' 2>/dev/null || true
}
wait_pod_running() { # <name> -> prints RUN once phase==Running
  for _ in $(seq 1 240); do
    R="$(pod_field "$1" 'p.get("status",{}).get("phase")=="Running" and "RUN"')"
    [[ "$R" == "RUN" ]] && { echo RUN; return 0; }
    sleep 0.5
  done
  return 1
}
wait_ready() { # <name> -> prints RDY once the Ready condition is True
  for _ in $(seq 1 120); do
    R="$(pod_field "$1" 'next((c.get("status") for c in p.get("status",{}).get("conditions",[]) if c.get("type")=="Ready"),"")=="True" and "RDY"')"
    [[ "$R" == "RDY" ]] && { echo RDY; return 0; }
    sleep 0.5
  done
  return 1
}
wait_restarts() { # <name> -> prints RC once containerStatuses restartCount >= 1
  for _ in $(seq 1 240); do
    R="$(pod_field "$1" 'next((c.get("restartCount",0) for c in p.get("status",{}).get("containerStatuses",[])),0)>=1 and "RC"')"
    [[ "$R" == "RC" ]] && { echo RC; return 0; }
    sleep 0.5
  done
  return 1
}
# Fast probe knobs so all quadrants resolve within seconds of RUNNING.
KNOBS='"initialDelaySeconds":1,"periodSeconds":1,"timeoutSeconds":1,"failureThreshold":1,"successThreshold":1'

# --- P1: readiness tcpSocket open port -> Ready --------------------------------
new_pod probe-tcp-rdy P1.json "{
  \"name\": \"c\", \"image\": \"$ECHO_REF\",
  \"readinessProbe\": {\"tcpSocket\": {\"port\": 8080}, $KNOBS}
}"
if [[ "$(wait_pod_running probe-tcp-rdy)" == "RUN" ]]; then
  if [[ "$(wait_ready probe-tcp-rdy)" == "RDY" ]]; then
    ok "P1 tcpSocket readiness on open :8080 -> Ready=True"
  else
    bad "P1 tcpSocket readiness never went Ready (open port)"
  fi
else
  bad "P1 pod probe-tcp-rdy never became Running"
fi

# --- P2: liveness tcpSocket closed port -> restart ------------------------------
# :9 (discard) is unbound in the echo container -> connect refused -> the
# liveness tracker crosses failureThreshold 1 -> the restart plan fires.
new_pod probe-tcp-dead P2.json "{
  \"name\": \"c\", \"image\": \"$ECHO_REF\",
  \"livenessProbe\": {\"tcpSocket\": {\"port\": 9}, $KNOBS}
}"
if [[ "$(wait_pod_running probe-tcp-dead)" == "RUN" ]]; then
  if [[ "$(wait_restarts probe-tcp-dead)" == "RC" ]]; then
    ok "P2 tcpSocket liveness on closed :9 -> restartCount >= 1"
  else
    bad "P2 tcpSocket liveness closed port never restarted the container"
  fi
else
  bad "P2 pod probe-tcp-dead never became Running"
fi

# --- P3: readiness exec rc==0 -> Ready ------------------------------------------
new_pod probe-exec-rdy P3.json "{
  \"name\": \"c\", \"image\": \"$ECHO_REF\",
  \"readinessProbe\": {\"exec\": {\"command\": [\"/bin/wfstep\", \"probe-ok\"]}, $KNOBS}
}"
if [[ "$(wait_pod_running probe-exec-rdy)" == "RUN" ]]; then
  if [[ "$(wait_ready probe-exec-rdy)" == "RDY" ]]; then
    ok "P3 exec readiness rc==0 (wfstep) -> Ready=True"
  else
    bad "P3 exec readiness never went Ready (rc==0 must be healthy)"
  fi
else
  bad "P3 pod probe-exec-rdy never became Running"
fi

# --- P4: liveness exec rc!=0 -> restart -----------------------------------------
new_pod probe-exec-dead P4.json "{
  \"name\": \"c\", \"image\": \"$ECHO_REF\",
  \"livenessProbe\": {\"exec\": {\"command\": [\"/bin/wfstep\", \"--fail\", \"probe-bad\"]}, $KNOBS}
}"
if [[ "$(wait_pod_running probe-exec-dead)" == "RUN" ]]; then
  if [[ "$(wait_restarts probe-exec-dead)" == "RC" ]]; then
    ok "P4 exec liveness rc!=0 (wfstep --fail) -> restartCount >= 1"
  else
    bad "P4 exec liveness rc!=0 never restarted the container"
  fi
else
  bad "P4 pod probe-exec-dead never became Running"
fi

# --- teardown hygiene (G25C idiom): converge to zero sandboxes BEFORE TERM ------
for p in probe-tcp-rdy probe-tcp-dead probe-exec-rdy probe-exec-dead; do
  curl -s -o /dev/null -X DELETE "$BASE/api/v1/namespaces/default/pods/$p" || true
done
for _ in $(seq 1 120); do
  CNT="$(INIT_PRO_DATA_DIR="$DD/agent" "$BIN" crictl pods -o json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null || echo 99)"
  [[ "$CNT" == "0" ]] && break
  sleep 0.5
done

echo
echo "probe e2e: $PASS passed, $FAIL failed"
if [[ "$FAIL" -ne 0 ]]; then
  echo "probe e2e FAILED" >&2
  exit 1
fi
echo "ALL probe e2e checks passed"
