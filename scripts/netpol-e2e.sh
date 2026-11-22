#!/usr/bin/env bash
# T4.4 (Q64): NetworkPolicy enforcement e2e over the real data plane.
# Boots a real single-node init-pro cluster (server + agent, vxlan flannel,
# same recipe as flannel-e2e.sh), runs 4 netpod pods, and asserts the
# node-side iptables keeper compiles networking.k8s.io/v1 policies:
#
#   NP1 default-deny ingress: an empty-podSelector policy isolates EVERY
#      pod in the namespace -> pod->pod connect is REJECTed (fail-fast,
#      refused - not a silent drop); plus the Q64 subset admission gate
#      (SCTP/named-port/ipBlock-mix rejections answer 422).
#   NP2 selected allow: policy selecting app=web allows ONLY app=api
#      peers on TCP/8080 -> api->web OK, client->web denied.
#   NP3 egress ipBlock: client's egress allows ONLY web's /32 ->
#      client->web OK, client->srv2:8081 denied.
#   NP4 zero-residue teardown: agent TERM -> the FORWARD jump and EVERY
#      INIT-PRO-NPOL-* chain are gone from iptables-save (the keeper's
#      teardown runs in the drain, before process exit).
#
# Evidence feeds decision Q64 (fallback in-tree iptables data plane).
#
# SKIP semantics (Q25/G24 parity): without root, a vendored containerd
# bundle, the cni tarball, /dev/net/tun, a C compiler, or iptables tools,
# the suite prints a SKIP line and exits 0 - a missing build input is a
# configuration state, not a conformance break.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl    >/dev/null || { echo "error: curl is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }

skip() { echo "SKIP netpol-e2e: $*"; exit 0; }

[[ "$(id -u)" == "0" ]] || skip "needs root (agent supervises containerd + iptables)"
CC="${CC:-cc}"
command -v "$CC" >/dev/null 2>&1 || skip "no C compiler for the offline netpod image"
[[ -e /dev/net/tun ]] || skip "no /dev/net/tun (flannel vxlan needs TUN/TAP device nodes)"
for t in iptables iptables-save iptables-restore; do
  command -v "$t" >/dev/null || skip "no $t on PATH (the T4.4 data plane shells out)"
done

BIN_DIR="$(dirname "$BIN")"
VENDOR_BIN=""
if [[ -n "${INIT_PRO_VENDOR_BIN:-}" && -f "$INIT_PRO_VENDOR_BIN/containerd" ]]; then
  VENDOR_BIN="$INIT_PRO_VENDOR_BIN"
else
  for c in "$BIN_DIR/../../vendor/bin" "$BIN_DIR/../vendor/bin" "$PWD/vendor/bin"; do
    [[ -f "$c/containerd" ]] && { VENDOR_BIN="$c"; break; }
  done
fi
[[ -n "$VENDOR_BIN" ]] || skip "no vendored containerd bundle (INIT_PRO_VENDOR_BIN=vendor/bin)"
CNI_TGZ_NAME="cni-plugins-linux-amd64-v1.5.1.tgz"
VENDOR_CACHE=""
if [[ -n "${INIT_PRO_VENDOR_CACHE:-}" && -f "$INIT_PRO_VENDOR_CACHE/$CNI_TGZ_NAME" ]]; then
  VENDOR_CACHE="$INIT_PRO_VENDOR_CACHE"
else
  for c in "$BIN_DIR/../../vendor/cache" "$BIN_DIR/../vendor/cache" "$PWD/vendor/cache"; do
    [[ -f "$c/$CNI_TGZ_NAME" ]] && { VENDOR_CACHE="$c"; break; }
  done
fi
[[ -n "$VENDOR_CACHE" ]] || skip "no vendored cni-plugins tarball ($CNI_TGZ_NAME)"

# T4.4 namespace hygiene: a leftover INIT-PRO-NPOL-* chain set from an
# earlier crashed run would fake NP4. Only OUR chain namespace is touched.
npol_chain_count() { iptables-save -t filter 2>/dev/null | grep -c '^:INIT-PRO-NPOL-' || true; }
if [[ "$(npol_chain_count)" != "0" ]]; then
  echo "netpol e2e: pre-cleaning $(npol_chain_count) leftover INIT-PRO-NPOL chains"
  iptables -D FORWARD -j INIT-PRO-NPOL-FWD 2>/dev/null || true
  for c in $(iptables-save -t filter 2>/dev/null | sed -n 's/^:\(INIT-PRO-NPOL-[A-Za-z0-9-]*\).*/\1/p'); do
    iptables -F "$c" 2>/dev/null || true
    iptables -X "$c" 2>/dev/null || true
  done
fi

pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
    || echo $((20000 + RANDOM % 10000))
}
stop_pid() { local p="$1"; [[ -n "$p" ]] || return 0; kill -TERM "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; }

PASS=0; FAIL=0
ok()  { echo "OK   $*"; PASS=$((PASS+1)); }
bad() { echo "FAIL $*"; FAIL=$((FAIL+1)); }

API_PORT="$(pick_port)"
KUBELET_PORT="$(pick_port)"
BASE="http://127.0.0.1:$API_PORT"
NODE_NAME="npol-node"
DD="$(mktemp -d /tmp/netpol-e2e.XXXXXX)"
SRV_LOG="$DD/server.log"; AG_LOG="$DD/agent.log"
SRV_PID=""; AGENT_PID=""
PAUSE_REF="init-pro.local/pause:0.1"
NETPOD_REF="init-pro.local/netpod:0.1"
NPOL_GROUP="apis/networking.k8s.io/v1/namespaces/default/networkpolicies"

CNI0_PREEXISTS=0
ip link show dev cni0 >/dev/null 2>&1 && CNI0_PREEXISTS=1
FLANNEL1_PREEXISTS=0
ip link show dev flannel.1 >/dev/null 2>&1 && FLANNEL1_PREEXISTS=1
if [[ "$CNI0_PREEXISTS" == "1" ]]; then
  CNI0_IP="$(ip -4 addr show dev cni0 2>/dev/null | grep -oE 'inet [0-9.]+' | awk '{print $2}' | head -1 || true)"
  if [[ -n "$CNI0_IP" && "${CNI0_IP%.*.*}" != "10.42" ]]; then
    skip "host cni0 holds $CNI0_IP - stale CNI state from another cluster"
  fi
fi

cleanup() {
  trap - EXIT INT TERM
  echo; echo "netpol e2e: tearing down ..."
  stop_pid "$AGENT_PID"; stop_pid "$SRV_PID"
  sleep 0.5
  for p in $(pgrep -f "$DD/.*containerd-shim" 2>/dev/null || true); do
    kill -KILL "$p" 2>/dev/null || true
  done
  for m in $(mount | awk -v dd="$DD/" 'index($3, dd) == 1 {print $3}'); do
    umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
  done
  rm -rf "$DD" 2>/dev/null || true
  if [[ "$CNI0_PREEXISTS" != "1" ]]; then ip link del cni0 2>/dev/null || true; fi
  if [[ "$FLANNEL1_PREEXISTS" != "1" ]]; then ip link del flannel.1 2>/dev/null || true; fi
  echo "netpol e2e: cleanup done"
}
trap cleanup EXIT INT TERM
trap 'exit 143' TERM

# --- offline images (Q27 recipe; shared deterministic builders) --------------
echo "netpol e2e: building images ..."
"$SCRIPT_DIR/build-netpod-image.sh" "$DD/netpod.tar" "$NETPOD_REF" >/dev/null
"$SCRIPT_DIR/build-pause-image.sh" "$DD/pause.tar" "$PAUSE_REF" >/dev/null

# --- cluster boot ------------------------------------------------------------
echo "netpol e2e: booting cluster (API $BASE, datadir $DD)"
"$BIN" server --data-dir "$DD/srv" --bind-address 127.0.0.1 \
  --https-listen-port "$API_PORT" --cluster-cidr 10.42.0.0/16 >"$SRV_LOG" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 60); do grep -q "discovery listening" "$SRV_LOG" && break; sleep 0.1; done
grep -q "discovery listening" "$SRV_LOG" || { echo "error: server never reported listening"; cat "$SRV_LOG" >&2; exit 1; }

INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" \
"$BIN" agent --data-dir "$DD/agent" --token dev \
  --server "$BASE" --node-name "$NODE_NAME" --node-ip 127.0.0.1 \
  --kubelet-port "$KUBELET_PORT" --flannel-backend vxlan >"$AG_LOG" 2>&1 &
AGENT_PID=$!
for _ in $(seq 1 300); do grep -q "containerd healthy" "$AG_LOG" && break; sleep 0.1; done
grep -q "containerd healthy" "$AG_LOG" || { echo "error: containerd never healthy"; cat "$AG_LOG" >&2; exit 1; }
grep -q "network policy enforcement started" "$AG_LOG" || bad "keeper startup line missing (agent log)"

INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$DD/pause.tar" >/dev/null
INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$DD/netpod.tar" >/dev/null

post_pod() { # name app command-json
  cat > "$DD/pod-$1.json" <<EOF
{"apiVersion":"v1","kind":"Pod","metadata":{"name":"$1","namespace":"default","labels":{"app":"$2"}},"spec":{"containers":[{"name":"netpod","image":"$NETPOD_REF","command":$3}]}}
EOF
  curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary @"$DD/pod-$1.json" "$BASE/api/v1/namespaces/default/pods"
}

wait_pod() { # name -> "<uid> <podIP>"
  for _ in $(seq 1 240); do
    curl -s "$BASE/api/v1/namespaces/default/pods/$1" | python3 -c 'import json,sys
try: p=json.load(sys.stdin)
except Exception: raise SystemExit(1)
st=p.get("status",{})
conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
uid=(p.get("metadata",{}).get("uid") or "")
ip=(st.get("podIP") or "")
if st.get("phase")=="Running" and conds.get("Ready")=="True" and uid and ip: print(uid, ip)
else: raise SystemExit(1)
' 2>/dev/null && break
    sleep 0.5
  done
}

# 4 pods: web serves 8080, srv2 serves 8081, api + client are idle probers.
[[ "$(post_pod web web '["/netpod","serve","8080"]')" == "201" ]] || { echo "error: pod web POST failed"; exit 1; }
[[ "$(post_pod srv2 srv2 '["/netpod","serve","8081"]')" == "201" ]] || { echo "error: pod srv2 POST failed"; exit 1; }
[[ "$(post_pod api api '["/netpod","idle"]')" == "201" ]] || { echo "error: pod api POST failed"; exit 1; }
[[ "$(post_pod client client '["/netpod","idle"]')" == "201" ]] || { echo "error: pod client POST failed"; exit 1; }
facts() { wait_pod "$1"; }
WEB="$(facts web)";   WEB_UID="${WEB%% *}";   WEB_IP="${WEB##* }"
SRV2="$(facts srv2)"; SRV2_UID="${SRV2%% *}"; SRV2_IP="${SRV2##* }"
API="$(facts api)";   API_UID="${API%% *}"
CLI="$(facts client)"; CLI_UID="${CLI%% *}"
[[ -n "$WEB_IP" && "$WEB_IP" != "$WEB_UID" && -n "$SRV2_IP" && "$SRV2_IP" != "$SRV2_UID" ]] \
  || { echo "error: pods never Ready with podIPs"; exit 1; }
ok "NP0 pods Running+Ready (web=$WEB_IP srv2=$SRV2_IP)"

# probe <uid> <ip> <port>: sets PROBE_CODE / PROBE_OUT from the exec reply
# (200 {"exitCode", "output"}; nonzero rc rides the body - Q31 exec wire).
probe() {
  local body
  body="$(curl -s --max-time 35 -X POST -H 'Content-Type: application/json' \
    -d "{\"command\":[\"/netpod\",\"get\",\"$2\",\"$3\"]}" \
    "http://127.0.0.1:$KUBELET_PORT/pods/$1/containers/netpod/exec" 2>/dev/null || true)"
  PROBE_CODE="$(python3 -c 'import json,sys
try: print(json.loads(sys.argv[1]).get("exitCode",""))
except Exception: pass' "$body" 2>/dev/null || true)"
  PROBE_OUT="$(python3 -c 'import json,sys
try: print(json.loads(sys.argv[1]).get("output",""))
except Exception: pass' "$body" 2>/dev/null || true)"
}

# wait_probe <uid> <ip> <port> <ok|denied> <secs>: poll until the probe
# reaches the wanted verdict. "denied" additionally requires the REJECT
# evidence (refused/unreachable in the output, nonzero rc) so a silent
# DROP can never pass for a fail-fast deny (Q64).
wait_probe() {
  local uid="$1" ip="$2" port="$3" want="$4" secs="${5:-45}" i
  for i in $(seq 1 "$secs"); do
    probe "$uid" "$ip" "$port"
    if [[ "$want" == ok ]]; then
      grep -q "netpod-ok" <<<"$PROBE_OUT" && return 0
    else
      if [[ "$PROBE_CODE" != "0" && -n "$PROBE_CODE" ]] \
         && grep -qiE "refused|unreachable" <<<"$PROBE_OUT"; then return 0; fi
    fi
    sleep 1
  done
  return 1
}

post_policy() { # file -> http code
  curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary @"$1" "$BASE/$NPOL_GROUP"
}
delete_policy() { # name -> http code
  curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/$NPOL_GROUP/$1"
}

# --- baseline: no policy -> pod->pod flows (flannel FLANNEL-FWD ACCEPTs) -----
if wait_probe "$CLI_UID" "$WEB_IP" 8080 ok 60; then
  ok "NP0 baseline client->web:8080 reachable with no policy"
else
  bad "NP0 baseline client->web:8080 unreachable BEFORE any policy (host FORWARD state?)"
fi

# --- NP1: default-deny ingress + admission-subset gate -----------------------
cat > "$DD/np-deny.json" <<'EOF'
{"apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicy","metadata":{"name":"np-deny-all","namespace":"default"},"spec":{"podSelector":{},"policyTypes":["Ingress"]}}
EOF
CODE="$(post_policy "$DD/np-deny.json")"
[[ "$CODE" == "201" ]] && ok "NP1 default-deny policy accepted (201)" \
                    || bad "NP1 policy POST -> $CODE (want 201)"
# Q64 subset gate: SCTP + named port + ipBlock-mix must answer 422.
printf '%s' '{"apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicy","metadata":{"name":"np-bad-sctp"},"spec":{"podSelector":{},"ingress":[{"ports":[{"protocol":"SCTP","port":80}]}]}}' > "$DD/np-bad1.json"
printf '%s' '{"apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicy","metadata":{"name":"np-bad-named"},"spec":{"podSelector":{},"ingress":[{"ports":[{"port":"http"}]}]}}' > "$DD/np-bad2.json"
printf '%s' '{"apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicy","metadata":{"name":"np-bad-mix"},"spec":{"podSelector":{},"ingress":[{"from":[{"ipBlock":{"cidr":"10.0.0.0/8"},"podSelector":{}}]}]}}' > "$DD/np-bad3.json"
SUBSET=1
for f in np-bad1 np-bad2 np-bad3; do
  C="$(post_policy "$DD/$f.json")"
  [[ "$C" == "422" ]] || { SUBSET=0; bad "NP1 subset gate $f -> $C (want 422)"; }
done
[[ "$SUBSET" == "1" ]] && ok "NP1 subset admission: SCTP/named-port/ipBlock-mix all 422 (Q64)"
# Enforcement: EVERY pod is ingress-isolated (empty podSelector); deny is
# fail-fast REJECT (refused), not a drop.
if wait_probe "$CLI_UID" "$WEB_IP" 8080 denied 45; then
  ok "NP1 client->web:8080 DENIED (refused) under default-deny"
else
  bad "NP1 client->web:8080 still reachable under default-deny (rc=${PROBE_CODE:-?} out=${PROBE_OUT:-none})"
fi
if wait_probe "$API_UID" "$WEB_IP" 8080 denied 30; then
  ok "NP1 api->web:8080 DENIED too (empty podSelector isolates ALL pods)"
else
  bad "NP1 api->web:8080 still reachable under default-deny"
fi

# --- NP2: selected allow (app=api peer, TCP/8080) ----------------------------
delete_policy np-deny-all >/dev/null
cat > "$DD/np-allow.json" <<EOF
{"apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicy","metadata":{"name":"np-allow","namespace":"default"},"spec":{"podSelector":{"matchLabels":{"app":"web"}},"ingress":[{"from":[{"podSelector":{"matchLabels":{"app":"api"}}}],"ports":[{"protocol":"TCP","port":8080}]}]}}
EOF
CODE="$(post_policy "$DD/np-allow.json")"
[[ "$CODE" == "201" ]] || bad "NP2 policy POST -> $CODE (want 201)"
if wait_probe "$API_UID" "$WEB_IP" 8080 ok 45; then
  ok "NP2 api->web:8080 ALLOWED (selected peer + port)"
else
  bad "NP2 api->web:8080 never reachable under allow policy (rc=${PROBE_CODE:-?} out=${PROBE_OUT:-none})"
fi
if wait_probe "$CLI_UID" "$WEB_IP" 8080 denied 30; then
  ok "NP2 client->web:8080 still DENIED (peer label does not match)"
else
  bad "NP2 client->web:8080 reachable under allow-from-api policy"
fi

# --- NP3: egress ipBlock allow-list on the client ----------------------------
delete_policy np-allow >/dev/null
cat > "$DD/np-egress.json" <<EOF
{"apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicy","metadata":{"name":"np-egress","namespace":"default"},"spec":{"podSelector":{"matchLabels":{"app":"client"}},"policyTypes":["Egress"],"egress":[{"to":[{"ipBlock":{"cidr":"$WEB_IP/32"}}]}]}}
EOF
CODE="$(post_policy "$DD/np-egress.json")"
[[ "$CODE" == "201" ]] || bad "NP3 egress policy POST -> $CODE (want 201)"
# web is unpoliced again; client may egress ONLY to web.
if wait_probe "$CLI_UID" "$WEB_IP" 8080 ok 45; then
  ok "NP3 client->web:8080 ALLOWED (ipBlock /32 allow-list)"
else
  bad "NP3 client->web:8080 never reachable under egress allow-list"
fi
if wait_probe "$CLI_UID" "$SRV2_IP" 8081 denied 45; then
  ok "NP3 client->srv2:8081 DENIED (destination outside the ipBlock)"
else
  bad "NP3 client->srv2:8081 reachable despite egress allow-list"
fi

# --- NP4: zero-residue teardown (drain-ordered, before process exit) ---------
delete_policy np-egress >/dev/null
# Wait one keeper resync so the empty program prunes every per-pod chain
# while the agent still runs (regen prune path), then TERM the agent.
sleep 6
GRACE_CHAINS="$(npol_chain_count)"
if [[ "$GRACE_CHAINS" == "1" ]]; then
  ok "NP4 after policy delete: only the dispatch chain left ($GRACE_CHAINS)"
else
  bad "NP4 after policy delete: $GRACE_CHAINS chains remain (want 1 dispatch only)"
fi
stop_pid "$AGENT_PID"; AGENT_PID=""
for _ in $(seq 1 60); do [[ "$(npol_chain_count)" == "0" ]] && break; sleep 0.5; done
RESIDUE="$(npol_chain_count)"
JUMP_ALIVE=0
iptables -C FORWARD -j INIT-PRO-NPOL-FWD >/dev/null 2>&1 && JUMP_ALIVE=1
TORN_LOG=0
grep -q "network policy torn down" "$AG_LOG" && TORN_LOG=1
if [[ "$RESIDUE" == "0" && "$JUMP_ALIVE" == "0" ]]; then
  ok "NP4 teardown: 0 INIT-PRO-NPOL chains, FORWARD jump gone (log-line=$TORN_LOG)"
else
  bad "NP4 teardown residue: $RESIDUE chains, jump_alive=$JUMP_ALIVE (log-line=$TORN_LOG)"
fi

# server teardown is trap-owned; summary before the trap fires
stop_pid "$SRV_PID"; SRV_PID=""
echo
echo "netpol e2e: $PASS passed, $FAIL failed"
if [[ "$FAIL" -ne 0 ]]; then
  echo "netpol e2e FAILED" >&2
  exit 1
fi
echo "ALL netpol e2e checks passed"
