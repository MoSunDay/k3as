#!/usr/bin/env bash
# T4.9: resources/QoS e2e. Boots a real single-node init-pro cluster
# (server + agent over the bundled containerd, T4.1/T4.2) and asserts that
# spec resources reach the CONTAINER'S OWN cgroup (read in-container via
# `/bin/wfcat` — the image has no shell/cat by design) and that
# `status.qosClass` follows the upstream GetPodQOS algorithm:
#
#   R1  limits enforced: cpu 250m req == limit + 64Mi mem limit ->
#       in-container /sys/fs/cgroup/cpu.max == "25000 100000" and
#       /sys/fs/cgroup/memory.max == "67108864" + qosClass Guaranteed
#   R2  BestEffort (no resources): cpu.max "max 100000", memory.max "max"
#       + qosClass BestEffort (quota -1 => no cap, memory omitted)
#   R3  Burstable (cpu request only): no cpu/mem cap + qosClass Burstable
#
# cgroup v2 only (in-container `memory.max`/`cpu.max` paths); a v1 host
# SKIPs (configuration state, not a conformance break). SKIP semantics
# otherwise mirror probe-e2e.sh (Q25 doctrine): no vendored containerd or
# `cc` for the offline image builder -> SKIP + exit 0. Requires a
# PRE-BUILT binary (cargo build), curl, python3, cc.
#
# Usage: scripts/resources-e2e.sh
#   INIT_PRO_BIN=...  reuse a specific binary (default target/debug/init-pro)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl    >/dev/null || { echo "error: curl is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }

skip() { echo "SKIP resources-e2e: $*"; exit 0; }

[[ "$(stat -fc %T /sys/fs/cgroup)" == "cgroup2fs" ]] || skip "host is not cgroup v2"

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
NODE_NAME="res-node"

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
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

echo "resources e2e: building the offline images (pause + echo/wfcat) ..."
"$SCRIPT_DIR/build-pause-image.sh" "$DD/pause.tar" "$PAUSE_REF" >/dev/null
"$SCRIPT_DIR/build-echo-image.sh" "$DD/echo.tar" "$ECHO_REF" >/dev/null

# --- cluster boot (probe-e2e coexistence recipe) --------------------------------
CLUSTER_CIDR="10.42.0.0/16"
CNI0_ADDR="$(ip -4 -o addr show dev cni0 2>/dev/null | awk '{print $4; exit}')"
if [[ "$CNI0_ADDR" =~ ^([0-9]+\.[0-9]+)\.[0-9]+\.[0-9]+/ ]]; then
  CLUSTER_CIDR="${BASH_REMATCH[1]}.0.0/16"
  echo "resources e2e: host cni0 is ${CNI0_ADDR}; reusing ${CLUSTER_CIDR}"
fi

echo "resources e2e: booting cluster (API $BASE, datadir $DD, cidr $CLUSTER_CIDR)"
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
container_id() { # <pod-name> -> the crictl container id for its (only) container
  INIT_PRO_DATA_DIR="$DD/agent" "$BIN" crictl ps --state Running -o json 2>/dev/null \
    | python3 -c '
import json,sys
rows = json.load(sys.stdin).get("containers", [])
mine = [c for c in rows if c.get("labels",{}).get("io.kubernetes.pod.name") == sys.argv[1]]
print(mine[0]["id"] if mine else "")' "$1" 2>/dev/null || true
}
cgroup_read() { # <pod-name> <cgroup-file> -> prints its in-container content
  local cid
  for _ in $(seq 1 20); do
    cid="$(container_id "$1")"
    [[ -n "$cid" ]] && break
    sleep 0.5
  done
  [[ -n "$cid" ]] || { echo "NOCONTAINER"; return 1; }
  INIT_PRO_DATA_DIR="$DD/agent" "$BIN" crictl exec --timeout 10 "$cid" /bin/wfcat "$2" 2>/dev/null \
    | tr -d '\n' || true
}

# --- R1: limits enforced in the container cgroup (Guaranteed) --------------------
new_pod res-limited R1.json "{
  \"name\": \"c\", \"image\": \"$ECHO_REF\",
  \"resources\": {
    \"requests\": {\"cpu\": \"250m\", \"memory\": \"64Mi\"},
    \"limits\":   {\"cpu\": \"250m\", \"memory\": \"64Mi\"}
  }
}"
if [[ "$(wait_pod_running res-limited)" == "RUN" ]]; then
  ok "R1a pod with limits reached Running"
  Q="$(pod_field res-limited 'p.get("status",{}).get("qosClass")=="Guaranteed" and "G"')"
  [[ "$Q" == "G" ]] && ok "R1b status.qosClass == Guaranteed (requests==limits)" \
                     || bad "R1b status.qosClass wrong: $(pod_field res-limited 'p.get("status",{}).get("qosClass")')"
  M="$(cgroup_read res-limited /sys/fs/cgroup/memory.max)"
  [[ "$M" == "67108864" ]] && ok "R1c in-container memory.max == 64Mi (67108864)" \
                          || bad "R1c in-container memory.max == '$M' (want 67108864)"
  C="$(cgroup_read res-limited /sys/fs/cgroup/cpu.max)"
  [[ "$C" == "25000 100000" ]] && ok "R1d in-container cpu.max == '25000 100000' (250m)" \
                             || bad "R1d in-container cpu.max == '$C' (want '25000 100000')"
else
  bad "R1 pod res-limited never became Running"
fi

# --- R2: BestEffort (no resources) -> no caps ------------------------------------
new_pod res-besteffort R2.json "{
  \"name\": \"c\", \"image\": \"$ECHO_REF\"
}"
if [[ "$(wait_pod_running res-besteffort)" == "RUN" ]]; then
  Q="$(pod_field res-besteffort 'p.get("status",{}).get("qosClass")=="BestEffort" and "B"')"
  [[ "$Q" == "B" ]] && ok "R2a status.qosClass == BestEffort" \
                     || bad "R2a status.qosClass wrong: $(pod_field res-besteffort 'p.get("status",{}).get("qosClass")')"
  M="$(cgroup_read res-besteffort /sys/fs/cgroup/memory.max)"
  [[ "$M" == "max" ]] && ok "R2b no memory cap (memory.max == max)" \
                     || bad "R2b memory.max == '$M' (want max)"
  C="$(cgroup_read res-besteffort /sys/fs/cgroup/cpu.max)"
  [[ "$C" == "max 100000" ]] && ok "R2c no cpu cap (cpu.max == 'max 100000')" \
                           || bad "R2c cpu.max == '$C' (want 'max 100000')"
else
  bad "R2 pod res-besteffort never became Running"
fi

# --- R3: Burstable (cpu request only) -> request noted, no caps ------------------
new_pod res-burstable R3.json "{
  \"name\": \"c\", \"image\": \"$ECHO_REF\",
  \"resources\": {\"requests\": {\"cpu\": \"100m\"}}
}"
if [[ "$(wait_pod_running res-burstable)" == "RUN" ]]; then
  Q="$(pod_field res-burstable 'p.get("status",{}).get("qosClass")=="Burstable" and "S"')"
  [[ "$Q" == "S" ]] && ok "R3a status.qosClass == Burstable" \
                     || bad "R3a status.qosClass wrong: $(pod_field res-burstable 'p.get("status",{}).get("qosClass")')"
  C="$(cgroup_read res-burstable /sys/fs/cgroup/cpu.max)"
  [[ "$C" == "max 100000" ]] && ok "R3b request-only keeps cpu uncapped (cpu.max == 'max 100000')" \
                           || bad "R3b cpu.max == '$C' (want 'max 100000')"
  M="$(cgroup_read res-burstable /sys/fs/cgroup/memory.max)"
  [[ "$M" == "max" ]] && ok "R3c request-only keeps memory uncapped (memory.max == max)" \
                     || bad "R3c memory.max == '$M' (want max)"
else
  bad "R3 pod res-burstable never became Running"
fi

# --- teardown hygiene (probe-e2e G25C idiom) -------------------------------------
for p in res-limited res-besteffort res-burstable; do
  curl -s -o /dev/null -X DELETE "$BASE/api/v1/namespaces/default/pods/$p" || true
done
for _ in $(seq 1 120); do
  CNT="$(INIT_PRO_DATA_DIR="$DD/agent" "$BIN" crictl pods -o json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null || echo 99)"
  [[ "$CNT" == "0" ]] && break
  sleep 0.5
done

echo
echo "resources e2e: $PASS passed, $FAIL failed"
if [[ "$FAIL" -ne 0 ]]; then
  echo "resources e2e FAILED" >&2
  exit 1
fi
echo "ALL resources e2e checks passed"
