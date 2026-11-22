#!/usr/bin/env bash
# T4.3 Scope A (Q34): pod-network e2e over the embedded flannel. Boots a real
# single-node init-pro cluster with `--cluster-cidr 10.42.0.0/16` (server) and
# `--flannel-backend vxlan` (agent), then asserts the full chain:
#
#   F1  server allocates a /24 podCIDR onto the node (controllers node_cidr)
#   F2  embedded flanneld annotates the node (flannel.alpha.coreos.com/*)
#       and clears NetworkUnavailable=False (kube subnet-mgr CompleteLease)
#   F3  a pod's podIP lands inside the node's podCIDR (flannel bridge CNI)
#   F4  pod->pod reachability over the vxlan fabric via kubelet exec
#   F5  note on upstream conformance (comment-only, see below)
#
# T4.3 remainder (Sprint 51) appended after the vxlan phase, same counters:
#   F6  host-gw single-node config plane over a SECOND fresh cluster
#       (net-conf.json body, node backend-type annotation, subnet.env,
#       podCIDR route via cni0) — agent `--flannel-backend host-gw`
#   F7/F8  two-node real-machine host-gw, ENV-GATED on
#       FLANNEL_E2E_REMOTE_NODE (ssh target, e.g. root@192.168.31.57):
#       F7 bidirectional /24 host routes to the peer node, F8 nodeName-
#       pinned pods with cross-node pod->pod reachability. Without the env
#       (or with an unreachable/unstaged remote) both SKIP, not FAIL.
#       FLANNEL_E2E_REMOTE_BIN (default /root/k3as-d/bin/init-pro) and
#       FLANNEL_E2E_REMOTE_DD (default /root/k3as-d/fn-remote) stage it.
#   F9  upstream-remainder note (wireguard backend informational; netpol is
#       T4.4/S52, not flannel)
#
# SKIP semantics (G24/G25 parity): without a vendored containerd bundle, the
# vendored cni-plugins tarball, root, /dev/net/tun, or `cc` for the offline
# image builder, the suite prints a SKIP line and exits 0 — a missing build
# input is a configuration state, not a conformance break.
#
# F5: upstream flannel ships a much wider suite (wireguard backend,
# netpol, more). This gate covers the k3as-relevant slices — single-node
# vxlan (F1-F5), host-gw config + gated two-node host-gw (F6-F8); wiring
# upstream's suite against the multicall `flannel` alias is future work,
# tracked in the T4.3 plan card (see F9 below).
#
# Usage: scripts/flannel-e2e.sh
#   INIT_PRO_BIN=...  reuse a specific binary (default target/debug/init-pro)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl   >/dev/null || { echo "error: curl is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }

skip() { echo "SKIP flannel-e2e: $*"; exit 0; }

# --- preflight (SKIP-grade inputs, same resolutions as the runtime crates) ---
[[ "$(id -u)" == "0" ]] || skip "needs root (agent supervises containerd + flannel owns the vxlan link)"
CC="${CC:-cc}"
command -v "$CC" >/dev/null 2>&1 || skip "no C compiler for the offline netpod image"
[[ -e /dev/net/tun ]] || skip "no /dev/net/tun (flannel vxlan needs TUN/TAP device nodes)"

BIN_DIR="$(dirname "$BIN")"
# vendor_bin_root resolution (runtime::stage): env override, exe-relative, cwd.
VENDOR_BIN=""
if [[ -n "${INIT_PRO_VENDOR_BIN:-}" && -f "$INIT_PRO_VENDOR_BIN/containerd" ]]; then
  VENDOR_BIN="$INIT_PRO_VENDOR_BIN"
else
  for c in "$BIN_DIR/../../vendor/bin" "$BIN_DIR/../vendor/bin" "$PWD/vendor/bin"; do
    [[ -f "$c/containerd" ]] && { VENDOR_BIN="$c"; break; }
  done
fi
[[ -n "$VENDOR_BIN" ]] || skip "no vendored containerd bundle (INIT_PRO_VENDOR_BIN=vendor/bin)"
# vendor_cache_dir resolution (runtime::stage): env override, exe-relative, cwd.
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
NODE_NAME="fn-node"
DD="$(mktemp -d /tmp/flannel-e2e.XXXXXX)"
SRV_LOG="$DD/server.log"; AG_LOG="$DD/agent.log"
SRV_PID=""; AGENT_PID=""
PAUSE_REF="init-pro.local/pause:0.1"
NETPOD_REF="init-pro.local/netpod:0.1"
# Phase 2 (F6-F8, S51): second fresh host-gw cluster + env-gated remote.
API2_PORT=""; KUBELET2_PORT=""; BASE2=""
NODE2_NAME="fn-node2"
DD2=""; SRV2_LOG=""; AG2_LOG=""
SRV2_PID=""; AGENT2_PID=""
REMOTE_NODE="${FLANNEL_E2E_REMOTE_NODE:-}"
REMOTE_BIN="${FLANNEL_E2E_REMOTE_BIN:-/root/k3as-d/bin/init-pro}"
REMOTE_DD="${FLANNEL_E2E_REMOTE_DD:-/root/k3as-d/fn-remote}"
REMOTE_PIDFILE=""

# CNI device pre-existence flags, S40c followup: probed BEFORE the trap is
# installed so the cleanup below only ever deletes devices THIS run created;
# a pre-existing cni0/flannel.1 (host / live cluster we coexist with) is
# left untouched.
CNI0_PREEXISTS=0
ip link show dev cni0 >/dev/null 2>&1 && CNI0_PREEXISTS=1
FLANNEL1_PREEXISTS=0
ip link show dev flannel.1 >/dev/null 2>&1 && FLANNEL1_PREEXISTS=1
# S40c coexistence: flannel needs the host bridge cni0 at 10.42.0.1/24. A
# foreign cni0 (live cluster with another CIDR) fails every RunPodSandbox,
# so SKIP with one actionable line instead of a battery of false FAILs.
if [[ "$CNI0_PREEXISTS" == "1" ]]; then
  CNI0_IP="$(ip -4 addr show dev cni0 2>/dev/null | grep -oE 'inet [0-9.]+' | awk '{print $2}' | head -1 || true)"
  if [[ -n "$CNI0_IP" && "${CNI0_IP%.*.*}" != "10.42" ]]; then
    skip "host cni0 holds $CNI0_IP — stale CNI state from another cluster (ip link delete cni0 to recover)"
  fi
fi

cleanup() {
  trap - EXIT INT TERM
  echo; echo "flannel e2e: tearing down ..."
  stop_pid "$AGENT_PID"; stop_pid "$SRV_PID"
  stop_pid "$AGENT2_PID"; stop_pid "$SRV2_PID"
  # F7/F8 remote-agent backstop: kill via the remote pidfile (best-effort).
  # Remote cni0/flannel state is RECORDED ONLY, never cleaned from here.
  if [[ -n "$REMOTE_PIDFILE" && -n "$REMOTE_NODE" ]]; then
    ssh -o BatchMode=yes -o ConnectTimeout=5 "$REMOTE_NODE" \
      "kill \$(cat '$REMOTE_PIDFILE' 2>/dev/null) 2>/dev/null; rm -f '$REMOTE_PIDFILE'" \
      >/dev/null 2>&1 || true
  fi
  sleep 0.5
  for p in $(pgrep -f "$DD/.*containerd-shim" 2>/dev/null || true); do
    kill -KILL "$p" 2>/dev/null || true
  done
  if [[ -n "$DD2" ]]; then
    for p in $(pgrep -f "$DD2/.*containerd-shim" 2>/dev/null || true); do
      kill -KILL "$p" 2>/dev/null || true
    done
  fi
  for m in $(mount | awk -v dd="$DD/" 'index($3, dd) == 1 {print $3}'); do
    umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
  done
  if [[ -n "$DD2" ]]; then
    for m in $(mount | awk -v dd="$DD2/" 'index($3, dd) == 1 {print $3}'); do
      umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
    done
  fi
  rm -rf "$DD" "$DD2" 2>/dev/null || true
  # CNI hygiene (S40c followup): remove devices THIS run created; a
  # pre-existing cni0/flannel.1 (host/live-cluster) is left untouched.
  if [[ "$CNI0_PREEXISTS" != "1" ]]; then ip link del cni0 2>/dev/null || true; fi
  if [[ "$FLANNEL1_PREEXISTS" != "1" ]]; then ip link del flannel.1 2>/dev/null || true; fi
  echo "flannel e2e: bye."
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# --- offline netpod image (deterministic OCI layout, Q27 recipe) -------------
# One static binary, three argv modes: serve <port> (echo responder),
# get <ip> <port> (connect+read, prints reply), idle (sleep forever, the
# image Entrypoint; pods override it via spec.containers[].command).
echo "flannel e2e: building the netpod image ..."
NET="$DD/netpod"; mkdir -p "$NET/src" "$NET/rootdir" "$NET/layout/blobs/sha256"
cat > "$NET/src/netpod.c" <<'EOF'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
static void serve(int port) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_addr.s_addr = htonl(INADDR_ANY); a.sin_port = htons((unsigned short)port);
    if (bind(s, (struct sockaddr*)&a, sizeof a) != 0) { perror("bind"); exit(1); }
    listen(s, 8);
    for (;;) {
        int c = accept(s, NULL, NULL);
        if (c < 0) continue;
        char buf[512]; ssize_t n = read(c, buf, sizeof buf - 1);
        if (n < 0) n = 0; buf[n > 0 ? n : 0] = 0;
        dprintf(c, "netpod-ok %s\n", buf);
        close(c);
    }
}
static int get(const char* ip, int port) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_port = htons((unsigned short)port);
    if (inet_pton(AF_INET, ip, &a.sin_addr) != 1) return 64;
    if (connect(s, (struct sockaddr*)&a, sizeof a) != 0) { perror("connect"); return 1; }
    dprintf(s, "ping\n");
    char buf[512]; ssize_t n = read(s, buf, sizeof buf - 1);
    if (n <= 0) return 2;
    buf[n] = 0; printf("%s", buf);
    return strncmp(buf, "netpod-ok", 9) == 0 ? 0 : 3;
}
int main(int argc, char** argv) {
    if (argc == 3 && !strcmp(argv[1], "serve")) { serve(atoi(argv[2])); return 0; }
    if (argc == 4 && !strcmp(argv[1], "get"))   { return get(argv[2], atoi(argv[3])); }
    if (argc == 2 && !strcmp(argv[1], "idle"))  { for (;;) pause(); }
    fprintf(stderr, "usage: netpod serve <port> | get <ip> <port> | idle\n");
    return 64;
}
EOF
(cd "$NET/src" && "$CC" -static -Os -o "$NET/rootdir/netpod" netpod.c)
sha256_of() { python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"; }
size_of()   { python3 -c 'import os,sys;print(os.path.getsize(sys.argv[1]))' "$1"; }
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
    -cf "$NET/layer.tar" -C "$NET/rootdir" .
gzip -n -9 -c "$NET/layer.tar" > "$NET/layer.tgz"
DIFF_ID="$(sha256_of "$NET/layer.tar")"
LAYER_DIGEST="$(sha256_of "$NET/layer.tgz")"; LAYER_SIZE="$(size_of "$NET/layer.tgz")"
printf '{"imageLayoutVersion":"1.0.0"}' > "$NET/layout/oci-layout"
cat > "$NET/config.json" <<EOF
{"architecture":"amd64","os":"linux","created":"1970-01-01T00:00:00Z","config":{"Entrypoint":["/netpod","idle"],"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],"User":"0:0","Labels":{"io.init-pro.built-by":"flannel-e2e.sh"}},"rootfs":{"type":"layers","diff_ids":["sha256:$DIFF_ID"]},"history":[{"created":"1970-01-01T00:00:00Z","created_by":"init-pro flannel e2e netpod"}]}
EOF
CONFIG_DIGEST="$(sha256_of "$NET/config.json")"; CONFIG_SIZE="$(size_of "$NET/config.json")"
cp "$NET/config.json" "$NET/layout/blobs/sha256/$CONFIG_DIGEST"
cp "$NET/layer.tgz"   "$NET/layout/blobs/sha256/$LAYER_DIGEST"
cat > "$NET/manifest.json" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:$CONFIG_DIGEST","size":$CONFIG_SIZE},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:$LAYER_DIGEST","size":$LAYER_SIZE}],"annotations":{"org.opencontainers.image.ref.name":"$NETPOD_REF"}}
EOF
MANIFEST_DIGEST="$(sha256_of "$NET/manifest.json")"; MANIFEST_SIZE="$(size_of "$NET/manifest.json")"
cp "$NET/manifest.json" "$NET/layout/blobs/sha256/$MANIFEST_DIGEST"
cat > "$NET/layout/index.json" <<EOF
{"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:$MANIFEST_DIGEST","size":$MANIFEST_SIZE,"annotations":{"org.opencontainers.image.ref.name":"$NETPOD_REF"}}]}
EOF
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
    -cf "$DD/netpod.tar" -C "$NET/layout" .
"$SCRIPT_DIR/build-pause-image.sh" "$DD/pause.tar" "$PAUSE_REF" >/dev/null

# --- cluster boot ------------------------------------------------------------
echo "flannel e2e: booting cluster (API $BASE, datadir $DD)"
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

INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$DD/pause.tar" >/dev/null
INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$DD/netpod.tar" >/dev/null

# --- F1: server allocates a /24 podCIDR from --cluster-cidr ------------------
F1=""
for _ in $(seq 1 120); do
  F1="$(curl -s "$BASE/api/v1/nodes/$NODE_NAME" | python3 -c 'import json,sys,ipaddress
try: n=json.load(sys.stdin)
except Exception: raise SystemExit
c=n.get("spec",{}).get("podCIDR") or ""
if not c: raise SystemExit
net=ipaddress.ip_network(c, strict=False)
cluster=ipaddress.ip_network("10.42.0.0/16")
if net.prefixlen==24 and net.subnet_of(cluster): print(c)' 2>/dev/null || true)"
  [[ -n "$F1" ]] && break
  sleep 0.5
done
if [[ -n "$F1" ]]; then ok "F1  node $NODE_NAME allocated podCIDR $F1 (a /24 of 10.42.0.0/16)"
else bad "F1  node never got a podCIDR (server: $SRV_LOG)"; fi

# --- F2: flannel node annotations + NetworkUnavailable=False -----------------
F2=""
for _ in $(seq 1 120); do
  F2="$(curl -s "$BASE/api/v1/nodes/$NODE_NAME" | python3 -c 'import json,sys
try: n=json.load(sys.stdin)
except Exception: raise SystemExit
a=n.get("metadata",{}).get("annotations",{}) or {}
p="flannel.alpha.coreos.com/"
conds={c.get("type"):c.get("status") for c in (n.get("status",{}).get("conditions") or [])}
if (a.get(p+"kube-subnet-manager")=="True" or a.get(p+"kube-subnet-manager")=="true") \
   and a.get(p+"backend-type")=="vxlan" \
   and a.get(p+"backend-data") and a.get(p+"public-ip") \
   and conds.get("NetworkUnavailable")=="False": print("ANN")' 2>/dev/null || true)"
  [[ "$F2" == "ANN" ]] && break
  sleep 0.5
done
if [[ "$F2" == "ANN" ]]; then
  ok "F2  flannel annotations present + NetworkUnavailable=False (kube subnet-mgr)"
else
  bad "F2  flannel node annotations/condition never converged"
fi

# --- pod A: serve 8080 (command override) ------------------------------------
cat > "$DD/podA.json" <<EOF
{"apiVersion":"v1","kind":"Pod","metadata":{"name":"netpod-a","namespace":"default","labels":{"app":"netpod"}},"spec":{"containers":[{"name":"netpod","image":"$NETPOD_REF","command":["/netpod","serve","8080"]}]}}
EOF
CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
  --data-binary @"$DD/podA.json" "$BASE/api/v1/namespaces/default/pods")"
[[ "$CODE" == "201" ]] || { echo "error: pod A POST -> $CODE"; exit 1; }

wait_pod_ready() { # base name -> prints "<uid> <podIP>" when Running+Ready
  for _ in $(seq 1 240); do
    curl -s "$1/api/v1/namespaces/default/pods/$2" | python3 -c 'import json,sys
try: p=json.load(sys.stdin)
except Exception: raise SystemExit(1)
st=p.get("status",{})
conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
uid=(p.get("metadata",{}).get("uid") or "")
ip=(st.get("podIP") or "")
if st.get("phase")=="Running" and conds.get("Ready")=="True" and uid and ip:
    print(uid, ip)
else:
    raise SystemExit(1)
' 2>/dev/null && break
    sleep 0.5
  done
}

A="$(wait_pod_ready "$BASE" netpod-a)"
A_UID="${A%% *}"; A_IP="${A##* }"
if [[ -n "$A_UID" && "$A_IP" != "$A_UID" ]]; then
  ok "F3a pod netpod-a Running+Ready with podIP $A_IP"
else
  bad "F3a pod netpod-a never became Running+Ready with a podIP"
fi

# --- F3: podIP inside the node podCIDR ---------------------------------------
if [[ -n "$F1" && -n "$A_IP" ]] && python3 -c "import ipaddress,sys
sys.exit(0 if ipaddress.ip_address(sys.argv[1]) in ipaddress.ip_network(sys.argv[2]) else 1)" "$A_IP" "$F1" 2>/dev/null; then
  ok "F3  podIP $A_IP is inside the node podCIDR $F1"
else
  bad "F3  podIP $A_IP is NOT inside the node podCIDR ${F1:-<none>}"
fi

# --- pod B: idle + kubelet exec get ------------------------------------------
cat > "$DD/podB.json" <<EOF
{"apiVersion":"v1","kind":"Pod","metadata":{"name":"netpod-b","namespace":"default","labels":{"app":"netpod"}},"spec":{"containers":[{"name":"netpod","image":"$NETPOD_REF"}]}}
EOF
CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
  --data-binary @"$DD/podB.json" "$BASE/api/v1/namespaces/default/pods")"
[[ "$CODE" == "201" ]] || { echo "error: pod B POST -> $CODE"; exit 1; }
B="$(wait_pod_ready "$BASE" netpod-b)"
B_UID="${B%% *}"

# --- F4: pod->pod reachability over the fabric (exec on B, dial A) -----------
F4=""
if [[ -n "$B_UID" && -n "$A_IP" ]]; then
  for _ in $(seq 1 60); do
    R="$(curl -s --max-time 35 -X POST -H 'Content-Type: application/json' \
      -d "{\"command\":[\"/netpod\",\"get\",\"$A_IP\",\"8080\"]}" \
      "http://127.0.0.1:$KUBELET_PORT/pods/$B_UID/containers/netpod/exec" 2>/dev/null || true)"
    grep -q "netpod-ok ping" <<<"$R" && { F4=1; break; }
    sleep 1
  done
fi
if [[ "${F4:-}" == "1" ]]; then
  ok "F4  exec(netpod-b) -> netpod-a:$A_IP:8080 answered over the flannel fabric"
else
  bad "F4  pod->pod exec probe never succeeded (reply: ${R:-<none>})"
fi

# --- F5: upstream-suite note (comment-only by design) ------------------------
echo "NOTE F5 upstream flannel conformance (host-gw/multi-node/netpol) is out of"
echo "        Scope A; see the T4.3 plan card. This gate covers the k3as slice."

# Teardown hygiene (G25C idiom): converge to zero sandboxes BEFORE TERM.
curl -s -o /dev/null -X DELETE "$BASE/api/v1/namespaces/default/pods/netpod-a" || true
curl -s -o /dev/null -X DELETE "$BASE/api/v1/namespaces/default/pods/netpod-b" || true
for _ in $(seq 1 120); do
  CNT="$(INIT_PRO_DATA_DIR="$DD/agent" "$BIN" crictl pods -o json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null || echo 99)"
  [[ "$CNT" == "0" ]] && break
  sleep 0.5
done

echo
echo "flannel e2e: vxlan phase (F1-F5) -> $PASS passed, $FAIL failed"

# =============================================================================
# Phase 2 — T4.3 remainder (Sprint 51): host-gw single-node config plane
# (F6), env-gated two-node host-gw (F7/F8), upstream note (F9).
# =============================================================================

# Local teardown of the vxlan cluster with the script's own stop functions
# (the EXIT trap stays the owner of final DD/device hygiene). cni0 is
# deliberately KEPT: the host-gw run below leases the same first /24 from
# the same --cluster-cidr out of its fresh store and reuses the bridge.
stop_pid "$AGENT_PID"; AGENT_PID=""
stop_pid "$SRV_PID"; SRV_PID=""
sleep 0.5
for p in $(pgrep -f "$DD/.*containerd-shim" 2>/dev/null || true); do
  kill -KILL "$p" 2>/dev/null || true
done
for m in $(mount | awk -v dd="$DD/" 'index($3, dd) == 1 {print $3}'); do
  umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
done

# --- F6: host-gw single-node config plane (fresh cluster, fresh datadir) ----
# Same boot idiom as phase 1, agent on `--flannel-backend host-gw`. The
# server binds 0.0.0.0 (not 127.0.0.1) so the env-gated F7/F8 remote agent
# can reach this exact API later; local curls keep using 127.0.0.1.
echo "flannel e2e: booting host-gw cluster (fresh datadir)"
API2_PORT="$(pick_port)"
KUBELET2_PORT="$(pick_port)"
BASE2="http://127.0.0.1:$API2_PORT"
DD2="$(mktemp -d /tmp/flannel-e2e-hg.XXXXXX)"
SRV2_LOG="$DD2/server.log"; AG2_LOG="$DD2/agent.log"
echo "flannel e2e: host-gw cluster API $BASE2, datadir $DD2"
"$BIN" server --data-dir "$DD2/srv" --bind-address 0.0.0.0 \
  --https-listen-port "$API2_PORT" --cluster-cidr 10.42.0.0/16 >"$SRV2_LOG" 2>&1 &
SRV2_PID=$!
for _ in $(seq 1 60); do grep -q "discovery listening" "$SRV2_LOG" && break; sleep 0.1; done
grep -q "discovery listening" "$SRV2_LOG" || { echo "error: host-gw server never reported listening"; cat "$SRV2_LOG" >&2; exit 1; }

INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" \
"$BIN" agent --data-dir "$DD2/agent" --token dev \
  --server "$BASE2" --node-name "$NODE_NAME" \
  --kubelet-port "$KUBELET2_PORT" --flannel-backend host-gw \
  --cluster-cidr 10.42.0.0/16 >"$AG2_LOG" 2>&1 &
AGENT2_PID=$!
for _ in $(seq 1 300); do grep -q "containerd healthy" "$AG2_LOG" && break; sleep 0.1; done
grep -q "containerd healthy" "$AG2_LOG" || { echo "error: host-gw containerd never healthy"; cat "$AG2_LOG" >&2; exit 1; }

# F6a: net-conf.json carries the synthesized host-gw body (the agent's
# flannel glue writes `<agent-data-dir>/agent/etc/flannel/net-conf.json` —
# the runtime's datadir convention; --flannel-conf would bypass it).
HG_NETCONF="$DD2/agent/agent/etc/flannel/net-conf.json"
HG_WANT_NETCONF='{"Backend":{"Type":"host-gw"},"Network":"10.42.0.0/16"}'
F6A=0
for _ in $(seq 1 120); do
  [[ -f "$HG_NETCONF" && "$(cat "$HG_NETCONF" 2>/dev/null || true)" == "$HG_WANT_NETCONF" ]] && { F6A=1; break; }
  sleep 0.5
done
if [[ "$F6A" -eq 1 ]]; then
  ok "F6a net-conf.json body == host-gw + 10.42.0.0/16 ($HG_NETCONF)"
else
  bad "F6a net-conf.json wrong/absent (want '$HG_WANT_NETCONF', got '$(cat "$HG_NETCONF" 2>/dev/null || true)')"
fi

# F6b: the node's flannel backend-type annotation says host-gw (flanneld
# leased as the kube subnet manager), and the /24 podCIDR is allocated.
HG_CIDR=""
F6B=0
for _ in $(seq 1 120); do
  NODE_JSON="$(curl -s "$BASE2/api/v1/nodes/$NODE_NAME" 2>/dev/null || true)"
  HG_CIDR="$(python3 -c 'import json,sys
try: n=json.load(sys.stdin)
except Exception: raise SystemExit
print(n.get("spec",{}).get("podCIDR") or "")' <<<"$NODE_JSON" 2>/dev/null || true)"
  BT="$(python3 -c 'import json,sys
try: n=json.load(sys.stdin)
except Exception: raise SystemExit
print((n.get("metadata",{}).get("annotations") or {}).get("flannel.alpha.coreos.com/backend-type") or "")' <<<"$NODE_JSON" 2>/dev/null || true)"
  if [[ "$BT" == "host-gw" && "$HG_CIDR" == 10.42.*.0/24 ]]; then F6B=1; break; fi
  sleep 0.5
done
if [[ "$F6B" -eq 1 ]]; then
  ok "F6b node annotated backend-type=host-gw with podCIDR $HG_CIDR"
else
  bad "F6b backend-type/podCIDR never converged (type: '${BT:-}', cidr: '${HG_CIDR:-}')"
fi

# F6c: flanneld wrote subnet.env (the CNI meta-plugin's FLANNEL_SUBNET_FILE).
F6C=0
for _ in $(seq 1 120); do
  if grep -q '^FLANNEL_NETWORK=10\.42\.0\.0' "$DD2/agent/agent/flannel/subnet.env" 2>/dev/null; then F6C=1; break; fi
  sleep 0.5
done
if [[ "$F6C" -eq 1 ]]; then
  ok "F6c subnet.env written with FLANNEL_NETWORK=10.42.0.0"
else
  bad "F6c subnet.env missing/short (agent log: $AG2_LOG)"
fi

# F6d: the node podCIDR has its host route via the cni0 bridge (survives
# from phase 1 — host-gw leases the same first /24; single-node host-gw
# carries no peer routes, those are the gated F7 evidence).
F6D=0
for _ in $(seq 1 120); do
  ip route show | grep -q "^$HG_CIDR dev cni0 " && { F6D=1; break; }
  sleep 0.5
done
if [[ "$F6D" -eq 1 ]]; then
  ok "F6d host route $HG_CIDR dev cni0 present"
else
  bad "F6d no '$HG_CIDR dev cni0' route (ip route: $(ip route show | head -3 | tr '\n' ';'))"
fi

# --- F7/F8: two-node real-machine host-gw (env-gated) -----------------------
# Gate notes: FLANNEL_E2E_REMOTE_NODE is an ssh target whose machine is
# pre-staged with the same-cached vendor bundle at FLANNEL_E2E_REMOTE_BIN
# and a writable FLANNEL_E2E_REMOTE_DD. Unreachable/unstaged => SKIP (the
# suite stays green); a REACHED machine with wrong assertions => FAIL.
if [[ -z "$REMOTE_NODE" ]]; then
  echo "SKIP F7/F8 (no FLANNEL_E2E_REMOTE_NODE — two-node host-gw needs a staged remote node; see gate notes)"
else
  REMOTE_HOST="${REMOTE_NODE##*@}"
  SSH_RC=0
  SSH_ERR="$(ssh -o BatchMode=yes -o ConnectTimeout=5 "$REMOTE_NODE" "test -x '$REMOTE_BIN'" 2>&1)" || SSH_RC=$?
  if [[ "$SSH_RC" -ne 0 ]]; then
    echo "SKIP F7/F8 (remote node unreachable/unstaged: ${SSH_ERR:-ssh rc=$SSH_RC ($REMOTE_BIN not staged)})"
  else
    # The local API URL as seen from the remote: the source address the
    # kernel would use towards it (the server above listens on 0.0.0.0).
    LOCAL_NODE_IP="$(ip -4 route get "$REMOTE_HOST" 2>/dev/null | grep -oE 'src [0-9.]+' | awk '{print $2}' | head -1 || true)"
    LOCAL_NODE_IP="${LOCAL_NODE_IP:-$(hostname -I 2>/dev/null | awk '{print $1}')}"
    BASE2_REMOTE="http://$LOCAL_NODE_IP:$API2_PORT"
    REMOTE_PIDFILE="$REMOTE_DD/agent.pid"
    # Stage the offline images (pause + netpod) into the remote containerd.
    ssh -o BatchMode=yes "$REMOTE_NODE" "mkdir -p '$REMOTE_DD'" >/dev/null 2>&1 || true
    scp -o BatchMode=yes -q "$DD/pause.tar" "$DD/netpod.tar" "$REMOTE_NODE:$REMOTE_DD/" 2>/dev/null \
      || { echo "SKIP F7/F8 (remote node unreachable/unstaged: image scp failed)"; REMOTE_NODE=""; }
    if [[ -n "$REMOTE_NODE" ]]; then
      ssh -o BatchMode=yes "$REMOTE_NODE" \
        "INIT_PRO_DATA_DIR='$REMOTE_DD/agent' '$REMOTE_BIN' ctr -n k8s.io images import '$REMOTE_DD/pause.tar' '$REMOTE_DD/netpod.tar' >/dev/null 2>&1" \
        || { echo "SKIP F7/F8 (remote node unreachable/unstaged: image import failed)"; REMOTE_NODE=""; }
    fi
    if [[ -n "$REMOTE_NODE" ]]; then
      ssh -o BatchMode=yes "$REMOTE_NODE" \
        "cd '$REMOTE_DD' && rm -rf '$REMOTE_DD/agent' && INIT_PRO_SANDBOX_IMAGE='$PAUSE_REF' \
nohup '$REMOTE_BIN' agent --data-dir '$REMOTE_DD/agent' --token dev \
  --server '$BASE2_REMOTE' --node-name '$NODE2_NAME' \
  --flannel-backend host-gw --cluster-cidr 10.42.0.0/16 \
  >'$REMOTE_DD/agent.log' 2>&1 & echo \$! > '$REMOTE_PIDFILE'" >/dev/null 2>&1 \
        || { echo "SKIP F7/F8 (remote node unreachable/unstaged: agent boot ssh failed)"; REMOTE_NODE=""; }
    fi
    if [[ -n "$REMOTE_NODE" ]]; then
      # F7: fn-node2 registers with its OWN /24 (distinct from node1's) and
      # BOTH sides carry the host-gw route to the peer's /24.
      R_CIDR=""
      for _ in $(seq 1 240); do
        R_CIDR="$(curl -s "$BASE2/api/v1/nodes/$NODE2_NAME" | python3 -c 'import json,sys
try: n=json.load(sys.stdin)
except Exception: raise SystemExit
print(n.get("spec",{}).get("podCIDR") or "")' 2>/dev/null || true)"
        [[ -n "$R_CIDR" && "$R_CIDR" != "$HG_CIDR" ]] && break
        sleep 0.5
      done
      if [[ -n "$R_CIDR" && "$R_CIDR" != "$HG_CIDR" ]]; then
        ok "F7  node $NODE2_NAME registered with its own podCIDR $R_CIDR (node1: $HG_CIDR)"
      else
        bad "F7  fn-node2 never registered with a distinct podCIDR (got '${R_CIDR:-}')"
      fi
      LOCAL_RT="$(ip route show | grep -c "$R_CIDR via $REMOTE_HOST" || true)"
      REMOTE_RT="$(ssh -o BatchMode=yes "$REMOTE_NODE" "ip route | grep -c '$HG_CIDR via $LOCAL_NODE_IP'" 2>/dev/null || echo 0)"
      if [[ "$LOCAL_RT" -ge 1 && "$REMOTE_RT" -ge 1 ]]; then
        ok "F7  host-gw routes both ways ($R_CIDR via $REMOTE_HOST local; $HG_CIDR via $LOCAL_NODE_IP remote)"
      else
        bad "F7  host routes missing (local hits: $LOCAL_RT, remote hits: $REMOTE_RT)"
      fi
      # F8: two single-replica Deployments, each pod pinned via
      # spec.nodeName to its node (anti-affinity not needed); both must go
      # Running+Ready with podIPs inside their node's podCIDR, then exec on
      # the local pod dials the remote pod over the host-gw fabric (F4
      # idiom).
      f8_dep() { # name nodename image -> Deployment JSON on stdout
        python3 - "$@" <<'PY'
import json, sys
name, node, image = sys.argv[1], sys.argv[2], sys.argv[3]
print(json.dumps({
  "apiVersion": "apps/v1", "kind": "Deployment",
  "metadata": {"name": name, "namespace": "default"},
  "spec": {"replicas": 1, "selector": {"matchLabels": {"app": name}},
           "template": {"metadata": {"labels": {"app": name}},
                        "spec": {"nodeName": node,
                                 "containers": [{"name": "netpod", "image": image,
                                                 "command": ["/netpod", "serve", "8080"]}]}}}}))
PY
      }
      for pair in "f8-a $NODE_NAME" "f8-b $NODE2_NAME"; do
        read -r dep node <<<"$pair"
        CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
          --data-binary "$(f8_dep "$dep" "$node" "$NETPOD_REF")" \
          "$BASE2/apis/apps/v1/namespaces/default/deployments" || true)"
        [[ "$CODE" == "201" ]] || bad "F8  deployment $dep POST -> $CODE"
      done
      # Deployment pod names carry a ReplicaSet hash, so resolve by label.
      wait_labeled_pod_ready() { # base label -> "<uid> <podIP>" when Ready
        for _ in $(seq 1 240); do
          curl -s "$1/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: raise SystemExit(1)
for p in items:
  if (p.get("metadata",{}).get("labels") or {}).get("app") != sys.argv[1]: continue
  st=p.get("status",{})
  conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
  uid=(p.get("metadata",{}).get("uid") or "")
  ip=(st.get("podIP") or "")
  if st.get("phase")=="Running" and conds.get("Ready")=="True" and uid and ip:
      print(uid, ip); break
' "$2" 2>/dev/null && break
          sleep 0.5
        done
      }
      A2="$(wait_labeled_pod_ready "$BASE2" f8-a)"
      B2="$(wait_labeled_pod_ready "$BASE2" f8-b)"
      F8_PODS=0
      if [[ -n "$A2" && -n "$B2" ]]; then
        A2_IP="${A2#* }"; B2_IP="${B2#* }"
        python3 -c "import ipaddress,sys
sys.exit(0 if ipaddress.ip_address(sys.argv[1]) in ipaddress.ip_network(sys.argv[2]) else 1)" "$A2_IP" "$HG_CIDR" 2>/dev/null \
          && python3 -c "import ipaddress,sys
sys.exit(0 if ipaddress.ip_address(sys.argv[1]) in ipaddress.ip_network(sys.argv[2]) else 1)" "$B2_IP" "$R_CIDR" 2>/dev/null \
          && F8_PODS=1
      fi
      if [[ "$F8_PODS" -eq 1 ]]; then
        ok "F8  pinned pods Ready with podIPs in their podCIDRs (f8-a-0 $A2_IP / f8-b-0 $B2_IP)"
      else
        bad "F8  pinned pods never Ready+in-CIDR (a: '${A2:-}', b: '${B2:-}')"
      fi
      F8=0
      if [[ "$F8_PODS" -eq 1 ]]; then
        A2_UID="${A2%% *}"
        for _ in $(seq 1 60); do
          R="$(curl -s --max-time 35 -X POST -H 'Content-Type: application/json' \
            -d "{\"command\":[\"/netpod\",\"get\",\"$B2_IP\",\"8080\"]}" \
            "http://127.0.0.1:$KUBELET2_PORT/pods/$A2_UID/containers/netpod/exec" 2>/dev/null || true)"
          grep -q "netpod-ok ping" <<<"$R" && { F8=1; break; }
          sleep 1
        done
      fi
      if [[ "$F8" -eq 1 ]]; then
        ok "F8  exec(f8-a-0) -> f8-b-0:$B2_IP:8080 answered across the host-gw fabric"
      else
        bad "F8  cross-node exec probe never succeeded (reply: ${R:-<none>})"
      fi
      # Remote teardown now (the cleanup-trap backstop repeats it best-effort).
      ssh -o BatchMode=yes "$REMOTE_NODE" \
        "kill \$(cat '$REMOTE_PIDFILE' 2>/dev/null) 2>/dev/null; rm -f '$REMOTE_PIDFILE'" >/dev/null 2>&1 || true
      REMOTE_PIDFILE=""
      # NOTE (record-only): the remote node's cni0/flannel state is left as-is.
    fi
  fi
fi

# --- F9: upstream-remainder note (informational, F5 posture) ----------------
# Beyond this gate (upstream flannel conformance): the wireguard backend is
# out of the T4.3 scope (informational); network policy is T4.4/S52, not a
# flannel concern.
ok "F9  upstream remainder noted (wireguard backend informational; netpol = T4.4)"

# Local teardown of the host-gw cluster before the summary (trap backstops).
stop_pid "$AGENT2_PID"; AGENT2_PID=""
stop_pid "$SRV2_PID"; SRV2_PID=""
sleep 0.5
for p in $(pgrep -f "$DD2/.*containerd-shim" 2>/dev/null || true); do
  kill -KILL "$p" 2>/dev/null || true
done
for m in $(mount | awk -v dd="$DD2/" 'index($3, dd) == 1 {print $3}'); do
  umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
done

echo
echo "flannel e2e: $PASS passed, $FAIL failed"
if [[ "$FAIL" -ne 0 ]]; then
  echo "flannel e2e FAILED" >&2
  exit 1
fi
echo "ALL flannel e2e checks passed"
