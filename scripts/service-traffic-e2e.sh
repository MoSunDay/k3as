#!/usr/bin/env bash
# Sprint 18 / S6: manifest-driven Service-traffic e2e suite. Boots a real
# single-node init-pro cluster (server + agent over the bundled containerd,
# T4.1/T4.2), builds the offline pause + echo images (S5, Q27-style), then
# drives Service traffic end-to-end through the built-in Router NodePort
# plane (S3 nodePort auto-allocation, S2 Endpoints targetPort resolution,
# S4 kube-proxy-equivalent listeners — decision D: NodePort-only service
# plane via the built-in Router, no ClusterIP dataplane; Q28 in the S7
# writeup). Modeled on k3s/Argo/OpenResty e2e conventions: manifest-driven
# (JSON-only wire, Q10), assertions numbered ST1..ST6.
#
# SKIP semantics (G24/G25 parity): without a vendored containerd bundle, or
# without `cc` for the offline image builders, the suite prints a SKIP line
# and exits 0 — a missing vendor bundle is a build-configuration state, not
# a conformance break. Requirements when NOT skipping: a PRE-BUILT binary
# (cargo build), curl, python3, cc, vendored containerd (vendor/bin).
#
# Usage: scripts/service-traffic-e2e.sh
#   INIT_PRO_BIN=...  reuse a specific binary (default target/debug/init-pro)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl   >/dev/null || { echo "error: curl is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }

ok()  { echo "OK   $*"; }

# Vendor detection mirrors runtime::stage::vendor_bin_root() and golden G24:
# INIT_PRO_VENDOR_BIN override, else exe-relative, else cwd-relative.
ST_VENDOR_BIN=""
if [[ -n "${INIT_PRO_VENDOR_BIN:-}" && -f "$INIT_PRO_VENDOR_BIN/containerd" ]]; then
  ST_VENDOR_BIN="$INIT_PRO_VENDOR_BIN"
else
  for c in "$(dirname "$BIN")/../../vendor/bin" "$(dirname "$BIN")/../vendor/bin" "$PWD/vendor/bin" "$ROOT/vendor/bin"; do
    if [[ -f "$c/containerd" ]]; then ST_VENDOR_BIN="$c"; break; fi
  done
fi
if [[ -z "$ST_VENDOR_BIN" ]]; then
  echo "SKIP service-traffic e2e (no vendored containerd; build with INIT_PRO_VENDOR=1)"
  exit 0
fi
if ! command -v cc >/dev/null 2>&1; then
  echo "SKIP service-traffic e2e (no cc for the offline image builders)"
  exit 0
fi

# Pick a free loopback port (avoids clashing with a host apiserver / prior run).
pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
    || echo $((20000 + RANDOM % 10000))
}

# TERM a pid (not INT — scripts may run under nohup/setsid where SIGINT is
# inherited-ignored), wait bounded, then force; reap either way.
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

API_PORT="$(pick_port)"
BASE="http://127.0.0.1:${API_PORT}"
DD="$(mktemp -d)"
SRV_LOG="$(mktemp)"; AG_LOG="$(mktemp)"
SRV_PID=""; AGENT_PID=""
PASS=0; FAIL=0

cleanup() {
  trap - EXIT
  [[ -n "$AGENT_PID" ]] && { stop_pid "$AGENT_PID"; AGENT_PID=""; }
  [[ -n "$SRV_PID" ]] && { stop_pid "$SRV_PID"; SRV_PID=""; }
  # Abnormal-path insurance (interrupted run / earlier FAIL with live pods):
  # kill any shim containerd left under $DD, then detach its mounts, so the
  # datadir removal below cannot hit busy entries.
  sleep 0.5
  for p in $(pgrep -f "$DD/.*containerd-shim" 2>/dev/null || true); do
    kill -KILL "$p" 2>/dev/null || true
  done
  # awk index() keeps the sweep pipefail-safe when nothing is mounted.
  for m in $(mount | awk -v dd="$DD/" 'index($3, dd) == 1 {print $3}'); do
    umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
  done
  # Tolerate leftovers regardless; never let the trap flip the exit status.
  rm -rf "$DD" "$SRV_LOG" "$AG_LOG" 2>/dev/null || true
}
trap cleanup EXIT
# Route fatal signals through the EXIT trap (bash skips it on untrapped
# signals); stop_pid then TERM-s the children (INT is unreliable under
# nohup/setsid, where it may be inherited-ignored).
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

echo "service-traffic e2e: booting cluster (API $BASE, datadir $DD)"
"$BIN" server --data-dir "$DD/srv" --bind-address 127.0.0.1 --https-listen-port "$API_PORT" >"$SRV_LOG" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 60); do grep -q "discovery listening" "$SRV_LOG" && break; sleep 0.1; done
grep -q "discovery listening" "$SRV_LOG" || { echo "error: server never reported listening"; cat "$SRV_LOG" >&2; exit 1; }

# Offline images (S5): pause for sandboxes (Q27), echo for the workload.
PAUSE_REF="init-pro.local/pause:0.1"
ECHO_REF="init-pro.local/echo:0.1"
"$SCRIPT_DIR/build-pause-image.sh" "$DD/pause.tar" "$PAUSE_REF" >/dev/null
"$SCRIPT_DIR/build-echo-image.sh" "$DD/echo.tar" "$ECHO_REF" >/dev/null

INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" "$BIN" agent --data-dir "$DD/agent" --token dev \
    --server "$BASE" --node-name st-node >"$AG_LOG" 2>&1 &
AGENT_PID=$!
for _ in $(seq 1 300); do grep -q "containerd healthy" "$AG_LOG" && break; sleep 0.1; done
grep -q "containerd healthy" "$AG_LOG" || { echo "error: containerd never healthy"; cat "$AG_LOG" >&2; exit 1; }
INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$DD/pause.tar" >/dev/null
INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$DD/echo.tar" >/dev/null

# Manifest-driven (e2e/manifests/*.json, JSON-only wire Q10), POSTed verbatim.
MANIFESTS="$ROOT/e2e/manifests"
CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
  --data-binary "@$MANIFESTS/echo-deployment.json" "$BASE/apis/apps/v1/namespaces/default/deployments")"
[[ "$CODE" == "201" ]] || { echo "error: deployment POST -> $CODE"; cat "$MANIFESTS/echo-deployment.json" >&2; exit 1; }
CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
  --data-binary "@$MANIFESTS/echo-service.json" "$BASE/api/v1/namespaces/default/services")"
[[ "$CODE" == "201" ]] || { echo "error: service POST -> $CODE"; cat "$MANIFESTS/echo-service.json" >&2; exit 1; }

# --- ST1: both replicas Running+Ready (phase + Ready condition, G25 idiom) ---
ST1=0
for _ in $(seq 1 240); do
  S="$(curl -s "$BASE/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
ready=0
for p in items:
  st=p.get("status",{})
  conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
  if st.get("phase")=="Running" and conds.get("Ready")=="True": ready+=1
if ready>=2: print("READY2")' 2>/dev/null || true)"
  [[ "$S" == "READY2" ]] && { ST1=1; break; }
  sleep 0.5
done
if [[ "$ST1" -eq 1 ]]; then
  ok "ST1  both echo pods Running+Ready on st-node"
  PASS=$((PASS+1))
else
  echo "FAIL ST1  pods never 2x Running+Ready (server: $SRV_LOG agent: $AG_LOG)"
  FAIL=$((FAIL+1))
fi

# --- ST2: Service got an auto-allocated nodePort in [30000, 32767] (S3) ---
NODE_PORT=""
if [[ "$ST1" -eq 1 ]]; then
  NODE_PORT="$(curl -s "$BASE/api/v1/namespaces/default/services/echo" | python3 -c 'import json,sys
try: svc=json.load(sys.stdin)
except Exception: sys.exit(0)
for p in svc.get("spec",{}).get("ports",[]):
  if p.get("name")=="web" and p.get("nodePort") is not None: print(p["nodePort"]); break' 2>/dev/null || true)"
fi
if [[ "$NODE_PORT" =~ ^[0-9]+$ ]] && (( NODE_PORT >= 30000 && NODE_PORT <= 32767 )); then
  ok "ST2  Service auto-allocated nodePort $NODE_PORT (range 30000-32767)"
  PASS=$((PASS+1))
else
  echo "FAIL ST2  no auto-allocated nodePort (got: '${NODE_PORT:-}')"
  FAIL=$((FAIL+1))
fi

# --- ST3: Endpoints parity — exactly the 2 podIPs, port 8080/name web (S2) ---
ST3=0
if [[ -n "$NODE_PORT" ]]; then
  curl -s "$BASE/api/v1/namespaces/default/endpoints/echo" -o "$DD/ep.json"
  S="$(curl -s "$BASE/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: pods=json.load(sys.stdin).get("items",[]); ep=json.load(open(sys.argv[1]))
except Exception: sys.exit(0)
ips={p.get("status",{}).get("podIP") for p in pods if p.get("status",{}).get("podIP")}
addrs=set(); ports=set()
for s in ep.get("subsets") or []:
  for a in s.get("addresses") or []:
    if a.get("ip"): addrs.add(a["ip"])
  for pt in s.get("ports") or []:
    ports.add((pt.get("port"), pt.get("name")))
if addrs==ips and len(addrs)==2 and (8080,"web") in ports: print("PARITY")' "$DD/ep.json" 2>/dev/null || true)"
  [[ "$S" == "PARITY" ]] && ST3=1
fi
if [[ "$ST3" -eq 1 ]]; then
  ok "ST3  Endpoints subsets carry exactly the 2 podIPs + port 8080/web"
  PASS=$((PASS+1))
else
  echo "FAIL ST3  Endpoints parity broken (endpoints: $DD/ep.json)"
  FAIL=$((FAIL+1))
fi

# --- ST4: 10 GETs round-robin across replicas + POST body echo ---
ST4=0
if [[ -n "$NODE_PORT" ]]; then
  # Bounded wait for the router reflector to pick up the Endpoints above.
  for _ in $(seq 1 60); do
    C="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$NODE_PORT/probe" 2>/dev/null || true)"
    [[ "$C" == "200" ]] && break
    sleep 0.5
  done
  : > "$DD/locals.txt"
  TRAFFIC_OK=1
  for _ in $(seq 1 10); do
    RESP="$(curl -s --max-time 5 -w $'\n%{http_code}' "http://127.0.0.1:$NODE_PORT/probe" 2>/dev/null || true)"
    C="${RESP##*$'\n'}"; BODY="${RESP%$'\n'*}"
    if [[ "$C" != "200" ]] || ! grep -q '^METHOD GET$' <<<"$BODY" || ! grep -q '^PATH /probe$' <<<"$BODY"; then
      TRAFFIC_OK=0; break
    fi
    grep '^LOCAL ' <<<"$BODY" >> "$DD/locals.txt" || true
  done
  DISTINCT="$(sort -u "$DD/locals.txt" | wc -l)"
  POSTED="$(curl -s --max-time 5 -X POST --data-binary 'sprint18' "http://127.0.0.1:$NODE_PORT/echo" 2>/dev/null || true)"
  if [[ "$TRAFFIC_OK" -eq 1 && "$DISTINCT" -ge 2 ]] && grep -q '^BODY sprint18$' <<<"$POSTED"; then
    ST4=1
  fi
fi
if [[ "$ST4" -eq 1 ]]; then
  ok "ST4  10 GETs 200 (METHOD GET + PATH /probe), $DISTINCT distinct LOCALs, POST body echoed"
  PASS=$((PASS+1))
else
  echo "FAIL ST4  traffic assertions (distinct LOCALs: ${DISTINCT:-0}; agent log: $AG_LOG)"
  FAIL=$((FAIL+1))
fi

# --- ST5: scale-to-zero converges to router 503 (empty Endpoints) ---
ST5=0
if [[ -n "$NODE_PORT" ]]; then
  curl -s "$BASE/apis/apps/v1/namespaces/default/deployments/echo" -o "$DD/dep.json"
  python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); d["spec"]["replicas"]=0
json.dump(d,open(sys.argv[1],"w"))' "$DD/dep.json"
  curl -s -o /dev/null -X PUT -H 'Content-Type: application/json' \
    --data-binary "@$DD/dep.json" "$BASE/apis/apps/v1/namespaces/default/deployments/echo"
  for _ in $(seq 1 120); do
    C="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$NODE_PORT/probe" 2>/dev/null || true)"
    [[ "$C" == "503" ]] && { ST5=1; break; }
    sleep 0.5
  done
fi
if [[ "$ST5" -eq 1 ]]; then
  ok "ST5  scale-to-zero converges: nodePort answers 503 (empty Endpoints)"
  PASS=$((PASS+1))
else
  echo "FAIL ST5  nodePort never converged to 503 after scale-to-zero"
  FAIL=$((FAIL+1))
fi

# --- ST6: Service delete retires the listener (connection refused) ---
ST6=0
if [[ -n "$NODE_PORT" ]]; then
  curl -s -o /dev/null -X DELETE "$BASE/api/v1/namespaces/default/services/echo"
  for _ in $(seq 1 60); do
    RC=0
    curl -s -o /dev/null --max-time 3 "http://127.0.0.1:$NODE_PORT/" 2>/dev/null || RC=$?
    [[ "$RC" == "7" ]] && { ST6=1; break; }   # 7 = connection refused
    sleep 0.5
  done
fi
if [[ "$ST6" -eq 1 ]]; then
  ok "ST6  Service delete retires the nodePort listener (connection refused)"
  PASS=$((PASS+1))
else
  echo "FAIL ST6  nodePort still reachable after Service delete"
  FAIL=$((FAIL+1))
fi

# Teardown hygiene (G25C idiom): converge to zero sandboxes BEFORE we TERM
# the agent, so containerd shuts down with no live tasks — no leaked shims,
# no busy shm/rootfs mounts in the datadir. Best-effort bounded: on the
# abnormal path (an earlier FAIL left pods alive) we time out and proceed.
for _ in $(seq 1 120); do
  CNT="$(INIT_PRO_DATA_DIR="$DD/agent" "$BIN" crictl pods -o json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null || echo 99)"
  [[ "$CNT" == "0" ]] && break
  sleep 0.5
done

echo
echo "service-traffic e2e: $PASS passed, $FAIL failed"
if [[ "$FAIL" -ne 0 ]]; then
  echo "service-traffic e2e FAILED" >&2
  exit 1
fi
echo "ALL service-traffic e2e checks passed"
