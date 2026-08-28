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
# SKIP semantics (G24/G25 parity): without a vendored containerd bundle, the
# vendored cni-plugins tarball, root, /dev/net/tun, or `cc` for the offline
# image builder, the suite prints a SKIP line and exits 0 — a missing build
# input is a configuration state, not a conformance break.
#
# F5: upstream flannel ships a much wider suite (host-gw/wireguard backends,
# multi-node vxlan, netpol). This gate covers the k3as-relevant single-node
# slice over the embedded daemon; wiring upstream's suite against the
# multicall `flannel` alias is future work, tracked in the T4.3 plan card.
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

cleanup() {
  trap - EXIT INT TERM
  echo; echo "flannel e2e: tearing down ..."
  stop_pid "$AGENT_PID"; stop_pid "$SRV_PID"
  sleep 0.5
  for p in $(pgrep -f "$DD/.*containerd-shim" 2>/dev/null || true); do
    kill -KILL "$p" 2>/dev/null || true
  done
  for m in $(mount | awk -v dd="$DD/" 'index($3, dd) == 1 {print $3}'); do
    umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
  done
  rm -rf "$DD" 2>/dev/null || true
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

wait_pod_ready() { # name -> prints "<uid> <podIP>" when Running+Ready
  for _ in $(seq 1 240); do
    curl -s "$BASE/api/v1/namespaces/default/pods/$1" | python3 -c 'import json,sys
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

A="$(wait_pod_ready netpod-a)"
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
B="$(wait_pod_ready netpod-b)"
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
echo "flannel e2e: $PASS passed, $FAIL failed"
if [[ "$FAIL" -ne 0 ]]; then
  echo "flannel e2e FAILED" >&2
  exit 1
fi
echo "ALL flannel e2e checks passed"
