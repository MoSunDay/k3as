#!/usr/bin/env bash
# A14: repeatable two-node acceptance gate for the k3as deployment
# (node02 control plane + node02 sidecar agent + node04 agent).
#
# Encodes the acceptance ledger of 2026-11-15 (验收报告.md):
#   A1  echo Deployment pods Running, podIP inside the node podCIDR
#   A2  pod logs relay (apiserver -> kubelet, Q31)
#   A3  pod exec relay with exit-code passthrough (Q31)
#   A4  both nodes Ready with distinct /24 podCIDRs
#   A5  host-gw: bidirectional LAN routes + pod<->pod TCP both ways (Q34)
#   A6  ClusterIP Service -> Endpoints convergence
#   A7  NodePort Service -> LAN round-robin across backends (Q28/Q41)
#   A8  Ingress REST round-trip + served-plane pin (host plane = T5.6 gap)
#   G4  Node annotation init-pro.io/kubelet-port matches the unit port
#   A12 Agent exec loop: Pending -> Succeeded + merge-patch write-back (Q37)
#   A13 Workflow template.agent -> Agent child scheduled + Succeeded (Q30/Q35)
#   T1  reverse tunnel relay: pod exec works with the kubelet port firewalled
#       from non-local sources (agent-dialed long-poll carries the hop, Q65)
#   T2  dropped-tunnel self-heal: SIGSTOP/SIGCONT, relay back <= 30s (Q65 SLA)
#   T3  --disable-proxy-tunnel drop-in: tunnel off, direct hops still ok
#   A9/A10 (opt-in ACCEPTANCE_RESTART=1): server/agent restart self-heal.
#   A11 (dual reboot) is MANUAL ONLY - never automated here.
#
# Idempotent: fixtures are created only when missing; A12/A13 objects are
# deleted + recreated each run. Runs ON node04 (the dev node) with passwordless
# SSH to node02. PASS/FAIL summary; exit != 0 on any FAIL.
#
# Usage: scripts/acceptance-2node-e2e.sh [ACCEPTANCE_RESTART=1]
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE="${INIT_PRO_SERVER:-http://192.168.31.57:6443}"
NODE02_IP="${K3AS_NODE02_IP:-192.168.31.57}"
NODE02_SSH="${K3AS_NODE02_SSH:-root@192.168.31.57}"
NODE04_NAME="${K3AS_NODE04_NAME:-node04}"
NODE04_CIDR="${K3AS_NODE04_CIDR:-10.244.0.0/24}"
AGENT_DD="${K3AS_AGENT_DD:-/root/k3as-d/agent}"
UNIT_AGENT="${K3AS_UNIT_AGENT:-/etc/systemd/system/k3as-agent.service}"
IMAGE_DIR="${K3AS_IMAGE_DIR:-/root/k3as-d/images}"
BIN="${INIT_PRO_BIN:-$ROOT/target/debug/init-pro}"
SNAP_DIR="${K3AS_SNAP_DIR:-/root/k3as-d/acceptance}"
RESTART_GATES="${ACCEPTANCE_RESTART:-0}"

PASS=0; FAIL=0
ok()  { echo "PASS $1"; PASS=$((PASS+1)); }
bad() { echo "FAIL $1"; FAIL=$((FAIL+1)); }
note() { echo "note: $*"; }
jqpy() { python3 -c "$1"; }

command -v curl python3 >/dev/null || { echo "error: curl/python3 required" >&2; exit 2; }
[[ -x "$BIN" ]] || { echo "error: $BIN missing (cargo build first)" >&2; exit 2; }

# ---------------------------------------------------------------- helpers ---
poll() { # poll SECS CMD... -> 0 when CMD (printed) becomes non-empty
  local secs="$1"; shift
  local i out
  for i in $(seq 1 "$secs"); do
    out="$("$@" 2>/dev/null || true)"
    [[ -n "$out" ]] && { echo "$out"; return 0; }
    sleep 1
  done
  return 1
}

pods_of() { # pods_of SELECTOR -> "name phase podIP nodeName" lines
  curl -s --max-time 5 "$BASE/api/v1/namespaces/default/pods" \
    | jqpy 'import json,sys
sel = "'"${1%%=*}"'" == "-" and None or "'"${1%%=*}"'"
want = "'"${1#*=}"'"
for p in json.load(sys.stdin).get("items", []):
    if sel is None or p["metadata"].get("labels", {}).get(sel) == want:
        s = p.get("status", {})
        print(p["metadata"]["name"], s.get("phase", ""), s.get("podIP", ""), p["spec"].get("nodeName", ""))' \
    2>/dev/null || true  # transient empty replies surface as python tracebacks; callers poll
}

ensure_obj() { # ensure_obj FILE URL -> 201/200 (create only when missing)
  local file="$1" url="$2" code
  code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$url")"
  if [[ "$code" == "200" ]]; then echo "200 (kept)"; return 0; fi
  code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
    -X POST -H 'Content-Type: application/json' --data-binary @"$file" "$url")"
  echo "$code"
}

# ------------------------------------------------------------------- A0 -----
mkdir -p "$SNAP_DIR"
{
  echo "# k3as acceptance snapshot $(date -Is)"
  systemctl cat k3as-agent 2>/dev/null
  ssh -o BatchMode=yes "$NODE02_SSH" 'systemctl cat k3as-server k3as-node-agent 2>/dev/null' \
    || note "A0: ssh snapshot of node02 units unavailable"
} > "$SNAP_DIR/units.txt" 2>/dev/null
sha256sum "$BIN" > "$SNAP_DIR/binary.sha256" 2>/dev/null
ok "A0  baseline snapshot -> $SNAP_DIR (units.txt, binary.sha256)"

# --------------------------------------------------------------- fixtures ---
if ! (INIT_PRO_DATA_DIR="$AGENT_DD" "$BIN" ctr -n k8s.io images ls 2>/dev/null \
      | grep -q 'init-pro.local/echo:0.1'); then
  [[ -f "$IMAGE_DIR/echo.tar" ]] || "$ROOT/scripts/build-echo-image.sh" "$IMAGE_DIR/echo.tar" >/dev/null
  [[ -f "$IMAGE_DIR/pause.tar" ]] || "$ROOT/scripts/build-pause-image.sh" "$IMAGE_DIR/pause.tar" >/dev/null
  INIT_PRO_DATA_DIR="$AGENT_DD" "$BIN" ctr -n k8s.io images import "$IMAGE_DIR/pause.tar" >/dev/null 2>&1
  INIT_PRO_DATA_DIR="$AGENT_DD" "$BIN" ctr -n k8s.io images import "$IMAGE_DIR/echo.tar" >/dev/null 2>&1
  INIT_PRO_DATA_DIR="$AGENT_DD" "$BIN" ctr -n k8s.io images tag \
    init-pro.local/pause:0.1 registry.k8s.io/pause:3.10 >/dev/null 2>&1
fi
if ! (INIT_PRO_DATA_DIR="$AGENT_DD" "$BIN" ctr -n k8s.io images ls 2>/dev/null \
      | grep -q 'init-pro.local/netpod:0.1'); then
  [[ -f "$IMAGE_DIR/netpod.tar" ]] || "$ROOT/scripts/build-netpod-image.sh" "$IMAGE_DIR/netpod.tar" >/dev/null
  INIT_PRO_DATA_DIR="$AGENT_DD" "$BIN" ctr -n k8s.io images import "$IMAGE_DIR/netpod.tar" >/dev/null 2>&1
  ssh -o BatchMode=yes "$NODE02_SSH" "test -d /root/k3as-d/node-agent" 2>/dev/null \
    && ssh -o BatchMode=yes "$NODE02_SSH" "INIT_PRO_DATA_DIR=/root/k3as-d/node-agent /root/k3as-d/bin/init-pro ctr -n k8s.io images import /root/k3as-d/images/netpod.tar" >/dev/null 2>&1
fi

C="$(ensure_obj "$ROOT/e2e/manifests/echo-deployment.json" "$BASE/apis/apps/v1/namespaces/default/deployments/echo")"
[[ "$C" == 20* ]] || bad "fixture echo deployment create -> $C"
C="$(ensure_obj "$ROOT/e2e/manifests/echo-service.json" "$BASE/api/v1/namespaces/default/services/echo")"
[[ "$C" == 20* ]] || bad "fixture echo nodePort service create -> $C"
for N in node02 node04; do
  cat > "/tmp/acc-netpod-$N.json" <<EOF
{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"netpod-$N","namespace":"default","labels":{"app":"netpod-$N"}},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"netpod-$N"}},"template":{"metadata":{"labels":{"app":"netpod-$N"}},"spec":{"nodeName":"$N","containers":[{"name":"netpod","image":"init-pro.local/netpod:0.1","command":["/netpod","serve","9999"]}]}}}}
EOF
  C="$(ensure_obj "/tmp/acc-netpod-$N.json" "$BASE/apis/apps/v1/namespaces/default/deployments/netpod-$N")"
  [[ "$C" == 20* ]] || bad "fixture netpod-$N create -> $C"
done
mkdir -p "$AGENT_DD/bin"
if [[ ! -x "$AGENT_DD/bin/opencoder" ]]; then
  cat > "$AGENT_DD/bin/opencoder" <<'EOF'
#!/bin/sh
export PATH=/bin:/usr/bin
task=""
for a in "$@"; do task="$a"; done
case "$task" in
  acc-fail) echo "[session acc-fail-s]" >&2; exit 7 ;;
  *) echo "marker $task" > marker.txt; echo "[session $task-s]" >&2; exit 0 ;;
esac
EOF
  chmod 755 "$AGENT_DD/bin/opencoder"
fi
cat > /tmp/acc-cip.json <<'EOF'
{"apiVersion":"v1","kind":"Service","metadata":{"name":"echo-cip","namespace":"default"},"spec":{"type":"ClusterIP","selector":{"app":"echo"},"ports":[{"name":"web","port":80,"targetPort":8080,"protocol":"TCP"}]}}
EOF
C="$(ensure_obj /tmp/acc-cip.json "$BASE/api/v1/namespaces/default/services/echo-cip")"
[[ "$C" == 20* ]] || bad "fixture echo-cip service create -> $C"
cat > /tmp/acc-ing.json <<'EOF'
{"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":"echo","namespace":"default"},"spec":{"rules":[{"host":"echo.k3as.local","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"echo","port":{"number":80}}}}]}}]}}
EOF
C="$(ensure_obj /tmp/acc-ing.json "$BASE/apis/networking.k8s.io/v1/namespaces/default/ingresses/echo")"
[[ "$C" == 20* ]] || bad "fixture ingress echo create -> $C"

# -------------------------------------------------------------------- A1 ----
A1_ROWS=""
for i in $(seq 1 90); do
  A1_ROWS="$(pods_of app=echo)"
  N_ALL=$(wc -l <<<"$A1_ROWS"); N_RUN=$(awk '$2=="Running"' <<<"$A1_ROWS" | wc -l)
  [[ "${N_ALL:-0}" -ge 1 && "$N_RUN" == "$N_ALL" ]] && break
  sleep 1
done
if [[ "${N_ALL:-0}" -ge 1 && "$N_RUN" == "$N_ALL" ]]; then
  IN_CIDR=1
  for ip in $(awk '{print $3}' <<<"$A1_ROWS"); do
    python3 -c "import sys,ipaddress; sys.exit(0 if ipaddress.ip_address('$ip') in ipaddress.ip_network('$NODE04_CIDR') else 1)" || IN_CIDR=0
  done
  IPS="$(awk '{print $3}' <<<"$A1_ROWS" | sort | paste -sd,)"
  [[ "$IN_CIDR" == 1 ]] && ok "A1  echo pods Running x$N_RUN, podIPs {$IPS} inside $NODE04_CIDR" \
                        || bad "A1  podIP(s) outside $NODE04_CIDR: $IPS"
else
  bad "A1  echo pods not all Running ($(tr '\n' ';' <<<"$A1_ROWS"))"
fi
POD="$(awk '$2=="Running"{print $1; exit}' <<<"$A1_ROWS")"

# ----------------------------------------------------------------- A2/A3 ----
LOG="$(curl -s --max-time 8 "$BASE/api/v1/namespaces/default/pods/$POD/log?tailLines=5")"
[[ -n "$LOG" && "$LOG" != *'"status":"Failure"'* ]] && ok "A2  logs relay: $(head -c 60 <<<"$LOG" | tr '\n' ' ')..." \
                                                 || bad "A2  logs relay empty/failure: $LOG"
EXEC="$(curl -s --max-time 8 -X POST -H 'Content-Type: application/json' \
  -d '{"command":["/bin/wfstep","hello"]}' "$BASE/api/v1/namespaces/default/pods/$POD/exec")"
if python3 -c 'import json,sys; d=json.loads(sys.argv[1]); sys.exit(0 if d.get("exitCode")==0 and "wfstep" in d.get("output","") else 1)' "$EXEC" 2>/dev/null; then
  ok "A3  exec relay: $EXEC"
else
  bad "A3  exec relay: $EXEC"
fi

# -------------------------------------------------------------------- A4 ----
# Poll: a freshly restarted agent needs ~40-70s to post first conditions
# (containerd supervise + CRI + registration before the first heartbeat).
nodes_snapshot() {
  curl -s --max-time 5 "$BASE/api/v1/nodes" | jqpy 'import json,sys
out=[]
for n in json.load(sys.stdin).get("items", []):
    conds={c["type"]:c["status"] for c in n.get("status",{}).get("conditions",[])}
    out.append(n["metadata"]["name"]+"="+conds.get("Ready","?")+"/"+(n.get("spec",{}).get("podCIDR") or "-"))
print(" ".join(sorted(out)))'
}
for _ in $(seq 1 30); do
  NODES="$(nodes_snapshot)"
  N_READY=$(grep -o '=True/' <<<"$NODES" | wc -l)
  N_CIDR=$(grep -oE '/10\.244\.[0-9]+\.0/24' <<<"$NODES" | sort -u | wc -l)
  [[ "$N_READY" -ge 2 && "$N_CIDR" -ge 2 ]] && break
  sleep 2
done
if [[ "$N_READY" -ge 2 && "$N_CIDR" -ge 2 ]]; then
  ok "A4  nodes Ready w/ distinct /24 podCIDRs: $NODES"
else
  bad "A4  nodes: $NODES (ready=$N_READY distinct-cidr=$N_CIDR)"
fi

# -------------------------------------------------------------------- A5 ----
R4="$(ip route | grep -c "10.244.1.0/24 via $NODE02_IP" || true)"
R2="$(ssh -o BatchMode=yes "$NODE02_SSH" "ip route | grep -c '10.244.0.0/24 via '" 2>/dev/null || echo 0)"
NP4=""
for i in $(seq 1 90); do
  NP4="$(pods_of "app=netpod-$NODE04_NAME" | awk -v n="$NODE04_NAME" '$2=="Running" && $4==n {print $3; exit}')"
  [[ -n "$NP4" ]] && break
  sleep 1
done
NP2=""
for i in $(seq 1 90); do
  NP2="$(pods_of app=netpod-node02 | awk '$2=="Running" && $4=="node02" {print $3; exit}')"
  [[ -n "$NP2" ]] && break
  sleep 1
done
NP4_NAME="$(pods_of app=netpod-$NODE04_NAME | awk '$2=="Running"{print $1; exit}')"
NP2_NAME="$(pods_of app=netpod-node02 | awk '$2=="Running"{print $1; exit}')"
if [[ "$R4" -ge 1 && "$R2" -ge 1 && -n "$NP4" && -n "$NP2" ]]; then
  ok "A5  host-gw routes both ways (node04:$R4 node02:$R2); netpods $NP4 / $NP2"
else
  bad "A5  routes/netpods missing (r4=$R4 r2=$R2 np4=$NP4 np2=$NP2)"
fi
if [[ -n "$NP4_NAME" && -n "$NP2_NAME" ]]; then
  G42="$(curl -s --max-time 10 -X POST -H 'Content-Type: application/json' \
    -d "{\"command\":[\"/netpod\",\"get\",\"$NP2\",\"9999\"]}" \
    "$BASE/api/v1/namespaces/default/pods/$NP4_NAME/exec")"
  G24="$(curl -s --max-time 10 -X POST -H 'Content-Type: application/json' \
    -d "{\"command\":[\"/netpod\",\"get\",\"$NP4\",\"9999\"]}" \
    "$BASE/api/v1/namespaces/default/pods/$NP2_NAME/exec")"
  if grep -q 'netpod-ok' <<<"$G42" && grep -q 'netpod-ok' <<<"$G24"; then
    ok "A5  pod<->pod TCP both directions ($G42 / $G24)"
  else
    bad "A5  cross-node TCP: n4->n2=$G42 n2->n4=$G24"
  fi
else
  bad "A5  netpod pod names unresolved (n4=$NP4_NAME n2=$NP2_NAME)"
fi

# -------------------------------------------------------------------- A6 ----
EP="$(curl -s --max-time 5 "$BASE/api/v1/namespaces/default/endpoints/echo-cip" \
  | jqpy 'import json,sys
e=json.load(sys.stdin); subs=e.get("subsets",[])
print(",".join(sorted(a["ip"] for s in subs for a in s.get("addresses",[]))))')"
EXP="$(pods_of app=echo | awk '$2=="Running"{print $3}' | sort | paste -sd,)"
[[ -n "$EP" && "$EP" == "$EXP" ]] && ok "A6  Endpoints converged: {$EP} == running pods {$EXP}" \
                                || bad "A6  endpoints {$EP} != pods {$EXP}"

# -------------------------------------------------------------------- A7 ----
NP="$(curl -s --max-time 5 "$BASE/api/v1/namespaces/default/services/echo" \
  | jqpy 'import json,sys
s=json.load(sys.stdin)
for p in s["spec"]["ports"]:
    if p.get("nodePort"): print(p["nodePort"]); break')"
if [[ -n "$NP" ]]; then
  RAW="$(for i in $(seq 1 10); do
    curl -s -m 5 "http://$NODE02_IP:$NP/probe" || true
    echo
  done)"
  ECHOC="$(grep -c '^ECHO ' <<<"$RAW" || true)"
  DISTINCT="$(grep '^LOCAL ' <<<"$RAW" | sort -u | wc -l)"
  BACKENDS="$(grep '^LOCAL ' <<<"$RAW" | sort -u | tr '\n' ' ')"
  if [[ "$ECHOC" == 10 && "$DISTINCT" -ge 2 ]]; then
    ok "A7  NodePort $NODE02_IP:$NP 10x GET ok, $DISTINCT backends: $BACKENDS"
  else
    bad "A7  nodePort 200s=$ECHOC/10 distinct=$DISTINCT ($BACKENDS)"
  fi
else
  bad "A7  no nodePort on service echo"
fi

# -------------------------------------------------------------------- A8 ----
ING="$(curl -s --max-time 5 "$BASE/apis/networking.k8s.io/v1/namespaces/default/ingresses/echo")"
HH404="$(curl -s -o /dev/null -w '%{http_code}' -m 5 -H 'Host: echo.k3as.local' "$BASE/")"
BEB="$(curl -s -m 5 -H 'Host: echo.k3as.local' "http://$NODE02_IP:$NP/probe" | head -1)"
if grep -q 'echo.k3as.local' <<<"$ING" && [[ "$HH404" == "404" ]] && grep -q '^ECHO ' <<<"$BEB"; then
  ok "A8  Ingress round-trip + served-plane pin (apiserver Host->$HH404 per T5.6; backend via nodePort: $BEB)"
else
  bad "A8  ingress=$ING host404=$HH404 backend=$BEB"
fi

# -------------------------------------------------------------------- G4 ----
UNIT_PORT="$(grep -oE '\-\-kubelet-port [0-9]+' "$UNIT_AGENT" 2>/dev/null | awk '{print $2}' | head -1)"
ANN="$(curl -s --max-time 5 "$BASE/api/v1/nodes/$NODE04_NAME" \
  | jqpy 'import json,sys; print(json.load(sys.stdin)["metadata"].get("annotations",{}).get("init-pro.io/kubelet-port",""))')"
[[ -n "$UNIT_PORT" && "$ANN" == "$UNIT_PORT" ]] \
  && ok "G4  kubelet-port annotation $ANN == unit port $UNIT_PORT (heartbeat reconcile, T4.2)" \
  || bad "G4  annotation '$ANN' != unit port '$UNIT_PORT'"

# ------------------------------------------------------------------- A12 ----
# Each run mints a unique Agent name and matches on the stable PROMPT
# marker, keeping the gate idempotent. (F4 hardened 2026-11-16: the runner
# dedupe is uid-scoped, so a recreated same-name object would also
# re-reconcile; unique names remain for run isolation.)
A12_NAME="acc-a12-$(date +%s)"
C="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' --max-time 5 \
  -d '{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"'"$A12_NAME"'","namespace":"default"},"spec":{"prompt":"acc-a12-run","nodeName":"'$NODE04_NAME'"}}' \
  "$BASE/apis/init-pro.io/v1/namespaces/default/agents")"
A12=""
for i in $(seq 1 90); do
  A12="$(curl -s --max-time 5 "$BASE/apis/init-pro.io/v1/namespaces/default/agents/$A12_NAME" \
    | jqpy 'import json,sys
a=json.load(sys.stdin); s=a.get("status",{})
c={x.get("type"):x for x in s.get("conditions",[])}
r=c.get("Ready",{}) or {}
if s.get("phase") in ("Succeeded","Failed"):
    print(s.get("phase"), r.get("reason",""), (r.get("message","") or ""))' 2>/dev/null || true)"
  [[ -n "$A12" ]] && break
  sleep 2
done
if grep -q '^Succeeded' <<<"$A12" && grep -q 'session acc-a12-run-s' <<<"$A12"; then
  ok "A12 Agent exec loop ($A12_NAME): $A12"
else
  bad "A12 Agent ($A12_NAME): ${A12:-no status}"
fi

# ------------------------------------------------------------------- A13 ----
A13_NAME="acc-a13-$(date +%s)"
A13_PROMPT="acc-a13-${A13_NAME##*-}-run"
C="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' --max-time 5 \
  -d '{"apiVersion":"argoproj.io/v1alpha1","kind":"Workflow","metadata":{"name":"'"$A13_NAME"'","namespace":"default"},"spec":{"entrypoint":"main","templates":[{"name":"main","agent":{"prompt":"'"$A13_PROMPT"'","model":{"name":"small"},"provider":"local"}}]}}' \
  "$BASE/apis/argoproj.io/v1alpha1/namespaces/default/workflows")"
A13=""
for i in $(seq 1 90); do
  A13="$(curl -s --max-time 5 "$BASE/apis/argoproj.io/v1alpha1/namespaces/default/workflows/$A13_NAME" \
    | jqpy 'import json,sys; print(json.load(sys.stdin).get("status",{}).get("phase",""))' 2>/dev/null || true)"
  [[ "$A13" == "Succeeded" || "$A13" == "Failed" || "$A13" == "Error" ]] && break
  sleep 2
done
AG13="$(curl -s --max-time 5 "$BASE/apis/init-pro.io/v1/namespaces/default/agents" \
  | jqpy 'import json,sys
for a in json.load(sys.stdin).get("items",[]):
    if a["spec"].get("prompt")=="'"$A13_PROMPT"'":
        print(a["spec"].get("nodeName",""), a.get("status",{}).get("phase",""))' 2>/dev/null || true)"
if [[ "$A13" == "Succeeded" && "$AG13" == "$NODE04_NAME Succeeded" ]]; then
  ok "A13 Workflow template.agent -> Agent($AG13) -> wf Succeeded ($A13_NAME)"
else
  bad "A13 wf=$A13 agent='$AG13' ($A13_NAME)"
fi

# ------------------------------------------------------- T1-T3 (T4.5/Q65) ---
# Reverse relay tunnel gates (agent-dialed; Q65; Q31 semantics locked):
#   T1  apiserver CANNOT dial the node04 kubelet (INPUT DROP on the
#       kubelet port for non-local sources) yet pod exec still works —
#       the request rides the node's outbound long-poll back.
#   T2  dropped-tunnel self-heal: SIGSTOP/SIGCONT the agent, measure the
#       wall clock until a relayed exec succeeds again (SLA <= 30s).
#   T3  `--disable-proxy-tunnel` (env drop-in): tunnel off, direct hops
#       keep working; flag removed again afterwards.
T_KPORT="${UNIT_PORT:-10250}"
T_SELF_IP="$(grep -oE '\-\-node-ip [0-9.]+' "$UNIT_AGENT" 2>/dev/null | awk '{print $2}' | head -1)"
T_SELF_IP="${T_SELF_IP:-$(hostname -I 2>/dev/null | awk '{print $1}')}"
np4_exec() { # -> exec JSON on the netpod-node04 pod (self-probe command)
  curl -s --max-time 25 -X POST -H 'Content-Type: application/json' \
    -d '{"command":["/netpod","get","127.0.0.1","9999"]}' \
    "$BASE/api/v1/namespaces/default/pods/$NP4_NAME/exec"
}
np4_exec_ok() {
  python3 -c 'import json,sys; d=json.loads(sys.argv[1]); sys.exit(0 if d.get("exitCode")==0 and "netpod" in d.get("output","").lower() else 1)' "$1" 2>/dev/null
}
t_drop_rule() { # t_drop_rule -I|-D : block the kubelet port for non-local sources
  # NOTE: iptables negation is its own argv token (`!` then `-s`), which is
  # exactly what lets the node-local dialer (tunnel execution) through while
  # the apiserver's direct dial is dropped.
  case "$1" in
    -I) iptables -I INPUT 1 '!' -s "$T_SELF_IP" -p tcp --dport "$T_KPORT" -j DROP 2>/dev/null ;;
    -D) iptables -D INPUT '!' -s "$T_SELF_IP" -p tcp --dport "$T_KPORT" -j DROP 2>/dev/null ;;
  esac
}

# -------------------------------------------------------------------- T1 ----
T1="skip"
if command -v iptables >/dev/null && [[ -n "$NP4_NAME" && -n "$T_SELF_IP" ]] && t_drop_rule -I; then
  TX="$(np4_exec)"
  if np4_exec_ok "$TX"; then
    ok "T1  relayed exec behind INPUT DROP (kubelet :$T_KPORT non-local): tunnel carried the hop"
    T1="pass"
  else
    bad "T1  relayed exec behind DROP failed: $TX"
    T1="fail"
  fi
else
  note "T1 skipped (iptables or NP4 pod unavailable; Q25)"
fi

# -------------------------------------------------------------------- T2 ----
if [[ "$T1" == "pass" ]] && systemctl is-active --quiet k3as-agent 2>/dev/null; then
  # --kill-who=main: freeze ONLY the agent process (dialer + kubelet) —
  # `all` would SIGSTOP containerd/shims and wound the running pods.
  systemctl kill -s SIGSTOP --kill-who=main k3as-agent 2>/dev/null
  sleep 30   # let the apiserver-side 25s park window expire (tunnel not live)
  systemctl kill -s SIGCONT --kill-who=main k3as-agent 2>/dev/null
  T0=$(date +%s); T2_OK=""; T2_S=""
  for i in $(seq 1 30); do
    T2_S="$(np4_exec)"
    if np4_exec_ok "$T2_S"; then T2_OK=1; break; fi
    sleep 1
  done
  T_ELAPSED=$(( $(date +%s) - T0 ))
  t_drop_rule -D
  if [[ -n "$T2_OK" && "$T_ELAPSED" -le 30 ]]; then
    ok "T2  dropped tunnel re-registered in ${T_ELAPSED}s (SLA <=30s, Q65)"
  else
    bad "T2  tunnel re-registration: ok=$T2_OK elapsed=${T_ELAPSED}s"
  fi
else
  t_drop_rule -D
  [[ "$T1" == "pass" ]] && note "T2 skipped (systemctl/agent unit unavailable; Q25)"
fi

# -------------------------------------------------------------------- T3 ----
T3_DROPIN="/etc/systemd/system/k3as-agent.service.d/acc-tunnel-off.conf"
if [[ -f "$UNIT_AGENT" ]] && systemctl is-active --quiet k3as-agent 2>/dev/null; then
  mkdir -p "${T3_DROPIN%/*}"
  # NOTE: clap v4 parses env bools strictly — only `true`/`false` (the
  # documented `--agent-require-sandbox` convention); `1` would exit(2).
  printf '[Service]\nEnvironment=INIT_PRO_DISABLE_PROXY_TUNNEL=true\n' > "$T3_DROPIN"
  systemctl daemon-reload && systemctl restart k3as-agent
  # Boot-race guard: the Node object's Ready stays stale-True across an
  # agent restart (lease expiry lags), so wait for the NEW process's own
  # boot marker instead — the tunnel-off line proves both the flag and
  # that the agent reached its spawn block (A10 notes 40-70s boots).
  T3_BOOTED=""
  for i in $(seq 1 60); do
    T3_BOOTED="$(journalctl -u k3as-agent --since '-3 min' --no-pager 2>/dev/null \
      | grep -c 'reverse relay tunnel off')"
    [[ "${T3_BOOTED:-0}" -ge 1 ]] && break
    sleep 2
  done
  T3_X=""; T3_OK=""
  for i in $(seq 1 45); do
    NP4_T3="$(pods_of "app=netpod-$NODE04_NAME" | awk -v n="$NODE04_NAME" '$2=="Running" && $4==n {print $1; exit}')"
    [[ -z "$NP4_T3" ]] && { sleep 2; continue; }
    T3_X="$(curl -s --max-time 20 -X POST -H 'Content-Type: application/json' \
      -d '{"command":["/netpod","get","127.0.0.1","9999"]}' \
      "$BASE/api/v1/namespaces/default/pods/$NP4_T3/exec")"
    if np4_exec_ok "$T3_X"; then T3_OK=1; break; fi
    sleep 2
  done
  rm -f "$T3_DROPIN"; systemctl daemon-reload && systemctl restart k3as-agent
  if [[ -n "$T3_OK" && "${T3_BOOTED:-0}" -ge 1 ]]; then
    ok "T3  --disable-proxy-tunnel: tunnel off (unit log x$T3_BOOTED), direct exec still ok"
  else
    bad "T3  disable-tunnel (booted=$T3_BOOTED exec-ok=$T3_OK exec=$T3_X)"
  fi
  for i in $(seq 1 60); do  # restore: wait Ready again before later gates
    R="$(curl -s --max-time 5 "$BASE/api/v1/nodes/$NODE04_NAME" \
      | jqpy 'import json,sys
n=json.load(sys.stdin); conds={c["type"]:c["status"] for c in n.get("status",{}).get("conditions",[])}
print(conds.get("Ready",""))' 2>/dev/null || true)"
    [[ "$R" == "True" ]] && break
    sleep 2
  done
else
  note "T3 skipped (systemd agent unit unavailable; Q25)"
fi


# ------------------------------------------------------- A9/A10 (opt-in) ----
if [[ "$RESTART_GATES" == "1" ]]; then
  BEFORE="$(curl -s --max-time 5 "$BASE/apis/apps/v1/namespaces/default/deployments" | jqpy 'import json,sys; print(",".join(sorted(i["metadata"]["name"] for i in json.load(sys.stdin).get("items",[]))))')"
  ssh -o BatchMode=yes "$NODE02_SSH" 'systemctl restart k3as-server'
  API_OK=""
  for i in $(seq 1 30); do
    [[ "$(curl -s -o /dev/null -w '%{http_code}' -m 2 "$BASE/api")" == "200" ]] && { API_OK=1; break; }
    sleep 1
  done
  AFTER="$(curl -s --max-time 5 "$BASE/apis/apps/v1/namespaces/default/deployments" | jqpy 'import json,sys; print(",".join(sorted(i["metadata"]["name"] for i in json.load(sys.stdin).get("items",[]))))')"
  [[ "$API_OK" == 1 && "$BEFORE" == "$AFTER" ]] \
    && ok "A9  server restart: api back, deployments intact ($AFTER)" \
    || bad "A9  server restart (api=$API_OK before=$BEFORE after=$AFTER)"
  systemctl restart k3as-agent
  RDY=""
  for i in $(seq 1 30); do
    RDY="$(curl -s --max-time 5 "$BASE/api/v1/nodes/$NODE04_NAME" \
      | jqpy 'import json,sys
n=json.load(sys.stdin); conds={c["type"]:c["status"] for c in n.get("status",{}).get("conditions",[])}
print(conds.get("Ready",""))' 2>/dev/null || true)"
    [[ "$RDY" == "True" ]] && break
    sleep 2
  done
  # Pods flip Pending while the kubelet re-creates sandboxes after its own
  # restart (new podIPs; v1 does not adopt live sandboxes) - poll to Running.
  PODS_LIVE=0
  for i in $(seq 1 45); do
    PODS_LIVE="$(pods_of app=echo | awk '$2=="Running"' | wc -l)"
    [[ "${PODS_LIVE:-0}" == 2 ]] && break
    sleep 2
  done
  RL=""
  for i in $(seq 1 15); do
    RL="$(curl -s --max-time 8 "$BASE/api/v1/namespaces/default/pods/$POD/log?tailLines=1")"
    [[ -n "$RL" && "$RL" != *'"Failure"'* ]] && break
    RL=""
    sleep 2
  done
  [[ "$RDY" == "True" && "${PODS_LIVE:-0}" == 2 && -n "$RL" ]] \
    && ok "A10 agent restart: Ready=$RDY pods Running=$PODS_LIVE relay ok" \
    || bad "A10 agent restart (ready=$RDY pods=${PODS_LIVE:-0} relay=${RL:-none})"
else
  note "A9/A10 skipped (set ACCEPTANCE_RESTART=1); A11 reboot stays manual"
fi

echo
echo "acceptance: $PASS passed, $FAIL failed (restart gates: $RESTART_GATES)"
[[ "$FAIL" == 0 ]]
