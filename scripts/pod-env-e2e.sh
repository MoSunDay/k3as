#!/usr/bin/env bash
# T4.7: container env/envFrom projection e2e. Boots a real single-node
# init-pro cluster (server + agent over the bundled containerd, T4.1/T4.2),
# builds an offline `envpod` image (static C: prints `ENV <k>=<v>` for its
# whole environ to stdout, then idles -- the flannel-e2e netpod recipe), and
# asserts the kubelet's create-time env resolution through the pod log
# subresource (the same `GET .../pods/<name>/log` call `kubectl logs` makes):
#
#   E1  literal env vars reach the container environment (logs)
#   E2  envFrom a pre-created ConfigMap projects its data keys (logs), and
#       an explicit env entry overrides an envFrom key of the same name
#       (upstream precedence: env beats envFrom)
#   E3  envFrom a pre-created Secret projects its data keys (logs)
#   E4  negative: a required (optional absent) configMapRef to a MISSING
#       ConfigMap never creates the container (pod stays non-Running) and
#       the apiserver carries the upstream-parity CreateContainerConfigError
#       event; the optional:true variant of the same ref DOES run without
#       contributing any variable
#
# Env is resolved at container CREATE time only (k8s parity; no hot update,
# no watch linkage) -- these cases pin exactly that contract.
#
# SKIP semantics (G24/G25 parity, Q25 doctrine): without a vendored
# containerd bundle or `cc` for the offline image builder, the suite prints
# a SKIP line and exits 0 -- a missing build input is a configuration state,
# not a conformance break. Requirements when NOT skipping: a PRE-BUILT
# binary (cargo build), curl, python3, cc, vendored containerd (vendor/bin).
#
# Usage: scripts/pod-env-e2e.sh
#   INIT_PRO_BIN=...  reuse a specific binary (default target/debug/init-pro)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl    >/dev/null || { echo "error: curl is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }

skip() { echo "SKIP pod-env-e2e: $*"; exit 0; }

# Vendor detection mirrors runtime::stage::vendor_bin_root() and golden G24:
# INIT_PRO_VENDOR_BIN override, else exe-relative, else cwd-relative.
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
command -v "$CC" >/dev/null 2>&1 || skip "no C compiler for the offline envpod image"

# Pick a free loopback port (avoids clashing with a host apiserver / prior run).
pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
    || echo $((20000 + RANDOM % 10000))
}
# TERM a pid (not INT -- scripts may run under nohup/setsid where SIGINT is
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

PASS=0; FAIL=0
ok()  { echo "OK   $*"; PASS=$((PASS+1)); }
bad() { echo "FAIL $*"; FAIL=$((FAIL+1)); }

API_PORT="$(pick_port)"
BASE="http://127.0.0.1:${API_PORT}"
DD="$(mktemp -d)"
SRV_LOG="$DD/server.log"; AG_LOG="$DD/agent.log"
SRV_PID=""; AGENT_PID=""
PAUSE_REF="init-pro.local/pause:0.1"
ENVPOD_REF="init-pro.local/envpod:0.1"
NODE_NAME="env-node"

cleanup() {
  trap - EXIT INT TERM HUP
  [[ -n "$AGENT_PID" ]] && stop_pid "$AGENT_PID"
  [[ -n "$SRV_PID" ]]   && stop_pid "$SRV_PID"
  # Abnormal-path insurance: kill shims under $DD, detach mounts, then rm.
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

# --- offline envpod image (deterministic OCI layout, Q27 recipe) -------------
# One static binary: dump the whole environment as `ENV <k>=<v>` lines, then
# idle forever (pods stay Running so the log subresource is always live).
echo "pod-env e2e: building the envpod image ..."
ENV="$DD/envpod"; mkdir -p "$ENV/src" "$ENV/rootdir" "$ENV/layout/blobs/sha256"
cat > "$ENV/src/envpod.c" <<'EOF'
#include <stdio.h>
#include <unistd.h>
extern char **environ;
int main(void) {
    for (char **e = environ; *e; e++) printf("ENV %s\n", *e);
    fflush(stdout);
    for (;;) pause();
}
EOF
(cd "$ENV/src" && "$CC" -static -Os -o "$ENV/rootdir/envpod" envpod.c)
[[ -x "$ENV/rootdir/envpod" ]] || { echo "error: envpod binary not built" >&2; exit 1; }
sha256_of() { python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"; }
size_of()   { python3 -c 'import os,sys;print(os.path.getsize(sys.argv[1]))' "$1"; }
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
    -cf "$ENV/layer.tar" -C "$ENV/rootdir" .
gzip -n -9 -c "$ENV/layer.tar" > "$ENV/layer.tgz"
DIFF_ID="$(sha256_of "$ENV/layer.tar")"
LAYER_DIGEST="$(sha256_of "$ENV/layer.tgz")"; LAYER_SIZE="$(size_of "$ENV/layer.tgz")"
printf '{"imageLayoutVersion":"1.0.0"}' > "$ENV/layout/oci-layout"
cat > "$ENV/config.json" <<EOF
{"architecture":"amd64","os":"linux","created":"1970-01-01T00:00:00Z","config":{"Entrypoint":["/envpod"],"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],"User":"0:0","Labels":{"io.init-pro.built-by":"pod-env-e2e.sh"}},"rootfs":{"type":"layers","diff_ids":["sha256:$DIFF_ID"]},"history":[{"created":"1970-01-01T00:00:00Z","created_by":"init-pro pod-env e2e envpod"}]}
EOF
CONFIG_DIGEST="$(sha256_of "$ENV/config.json")"; CONFIG_SIZE="$(size_of "$ENV/config.json")"
cp "$ENV/config.json" "$ENV/layout/blobs/sha256/$CONFIG_DIGEST"
cp "$ENV/layer.tgz"   "$ENV/layout/blobs/sha256/$LAYER_DIGEST"
cat > "$ENV/manifest.json" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:$CONFIG_DIGEST","size":$CONFIG_SIZE},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:$LAYER_DIGEST","size":$LAYER_SIZE}],"annotations":{"org.opencontainers.image.ref.name":"$ENVPOD_REF"}}
EOF
MANIFEST_DIGEST="$(sha256_of "$ENV/manifest.json")"; MANIFEST_SIZE="$(size_of "$ENV/manifest.json")"
cp "$ENV/manifest.json" "$ENV/layout/blobs/sha256/$MANIFEST_DIGEST"
cat > "$ENV/layout/index.json" <<EOF
{"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:$MANIFEST_DIGEST","size":$MANIFEST_SIZE,"annotations":{"org.opencontainers.image.ref.name":"$ENVPOD_REF"}}]}
EOF
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
    -cf "$DD/envpod.tar" -C "$ENV/layout" .
"$SCRIPT_DIR/build-pause-image.sh" "$DD/pause.tar" "$PAUSE_REF" >/dev/null

# --- cluster boot ------------------------------------------------------------
# Cluster CIDR for the throwaway cluster: the embedded flanneld programs
# the shared host bridge `cni0` with the gateway of this node's leased
# /24, and the vendored bridge plugin errors when a pre-existing cni0
# carries a different address (e.g. this host also runs a live init-pro
# agent). When cni0 already exists, reuse ITS /16 so the gateway matches;
# otherwise the default 10.42.0.0/16 (clean-host path).
CLUSTER_CIDR="10.42.0.0/16"
CNI0_ADDR="$(ip -4 -o addr show dev cni0 2>/dev/null | awk '{print $4; exit}')"
if [[ "$CNI0_ADDR" =~ ^([0-9]+\.[0-9]+)\.[0-9]+\.[0-9]+/ ]]; then
  CLUSTER_CIDR="${BASH_REMATCH[1]}.0.0/16"
  echo "pod-env e2e: host cni0 is ${CNI0_ADDR}; reusing ${CLUSTER_CIDR}"
fi

echo "pod-env e2e: booting cluster (API $BASE, datadir $DD, cidr $CLUSTER_CIDR)"
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
INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$DD/envpod.tar" >/dev/null

# --- helpers ------------------------------------------------------------------
post_json() { # <path> <file> -> HTTP code
  curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary @"$2" "$BASE$1"
}
wait_pod_running() { # <name> -> prints "RUN" once phase==Running
  for _ in $(seq 1 240); do
    R="$(curl -s "$BASE/api/v1/namespaces/default/pods/$1" | python3 -c 'import json,sys
try: p=json.load(sys.stdin)
except Exception: raise SystemExit(1)
print("RUN" if p.get("status",{}).get("phase")=="Running" else "")' 2>/dev/null || true)"
    [[ "$R" == "RUN" ]] && { echo RUN; return 0; }
    sleep 0.5
  done
  return 1
}
pod_log() { # <pod> -> the container log text, CRI framing stripped
  # The kubelet log subresource returns containerd's raw CRI log file, where
  # every line is framed `<RFC3339 timestamp> <stdout|stderr> <F|P> <text>`;
  # strip the 3 leading fields so the greps below match `^ENV ...$` exactly
  # (the same framing acceptance-2node-e2e.sh A2 tolerates implicitly).
  curl -s "$BASE/api/v1/namespaces/default/pods/$1/log?container=envpod" \
    | sed 's/^[^ ]* [^ ]* [^ ]* //'
}
wait_event() { # <pod> <reason> -> prints "EV" once the event exists
  for _ in $(seq 1 120); do
    R="$(curl -s "$BASE/api/v1/namespaces/default/events" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: raise SystemExit(1)
want_pod, want_reason = sys.argv[1], sys.argv[2]
for ev in items:
    io = ev.get("involvedObject",{}) or {}
    if io.get("name")==want_pod and ev.get("reason")==want_reason:
        print("EV"); raise SystemExit(0)
raise SystemExit(1)' "$1" "$2" 2>/dev/null || true)"
    [[ "$R" == "EV" ]] && { echo EV; return 0; }
    sleep 0.5
  done
  return 1
}
new_pod() { # <file> <json> -> POSTs it or dies
  printf '%s' "$3" > "$DD/$2"
  CODE="$(post_json "/api/v1/namespaces/default/pods" "$DD/$2")"
  [[ "$CODE" == "201" ]] || { echo "error: pod $1 POST -> $CODE" >&2; exit 1; }
}

# --- E1: literal env vars reach the container ---------------------------------
new_pod envpod-lit podE1.json '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"envpod-lit","namespace":"default"},"spec":{"containers":[{"name":"envpod","image":"'"$ENVPOD_REF"'","env":[{"name":"E1_A","value":"hello-literal"},{"name":"E1_B","value":"42"}]}]}}'
if [[ "$(wait_pod_running envpod-lit)" == "RUN" ]]; then
  LOG="$(pod_log envpod-lit)"
  if grep -q '^ENV E1_A=hello-literal$' <<<"$LOG" && grep -q '^ENV E1_B=42$' <<<"$LOG"; then
    ok "E1  literal env vars visible in the container logs (printenv-style)"
  else
    bad "E1  literal env vars missing from logs: $(grep '^ENV E1_' <<<"$LOG" | tr '\n' ' ')"
  fi
else
  bad "E1  pod envpod-lit never became Running"
fi

# --- E2: envFrom ConfigMap (+ explicit env overrides the same key) ------------
printf '%s' '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"envpod-cm","namespace":"default"},"data":{"E2_FROM":"from-configmap","E2_ONLY":"cm-val"}}' > "$DD/cm.json"
CODE="$(post_json /api/v1/namespaces/default/configmaps "$DD/cm.json")"
[[ "$CODE" == "201" ]] || { echo "error: configmap POST -> $CODE" >&2; exit 1; }
new_pod envpod-cm podE2.json '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"envpod-cm","namespace":"default"},"spec":{"containers":[{"name":"envpod","image":"'"$ENVPOD_REF"'","env":[{"name":"E2_FROM","value":"explicit-wins"}],"envFrom":[{"configMapRef":{"name":"envpod-cm"}}]}]}}'
if [[ "$(wait_pod_running envpod-cm)" == "RUN" ]]; then
  LOG="$(pod_log envpod-cm)"
  if grep -q '^ENV E2_ONLY=cm-val$' <<<"$LOG" \
     && grep -q '^ENV E2_FROM=explicit-wins$' <<<"$LOG" \
     && ! grep -q '^ENV E2_FROM=from-configmap$' <<<"$LOG"; then
    ok "E2  envFrom ConfigMap projected + explicit env overrides duplicate names"
  else
    bad "E2  configMap envFrom projection wrong: $(grep '^ENV E2_' <<<"$LOG" | tr '\n' ' ')"
  fi
else
  bad "E2  pod envpod-cm never became Running"
fi

# --- E3: envFrom Secret --------------------------------------------------------
printf '%s' '{"apiVersion":"v1","kind":"Secret","metadata":{"name":"envpod-secret","namespace":"default"},"data":{"E3_TOKEN":"e2e-plain-value","E3_ONLY":"sec-val"}}' > "$DD/sec.json"
CODE="$(post_json /api/v1/namespaces/default/secrets "$DD/sec.json")"
[[ "$CODE" == "201" ]] || { echo "error: secret POST -> $CODE" >&2; exit 1; }
new_pod envpod-sec podE3.json '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"envpod-sec","namespace":"default"},"spec":{"containers":[{"name":"envpod","image":"'"$ENVPOD_REF"'","envFrom":[{"secretRef":{"name":"envpod-secret"}}]}]}}'
if [[ "$(wait_pod_running envpod-sec)" == "RUN" ]]; then
  LOG="$(pod_log envpod-sec)"
  if grep -q '^ENV E3_TOKEN=e2e-plain-value$' <<<"$LOG" && grep -q '^ENV E3_ONLY=sec-val$' <<<"$LOG"; then
    ok "E3  envFrom Secret projected (plain-string data convention)"
  else
    bad "E3  secret envFrom projection wrong: $(grep '^ENV E3_' <<<"$LOG" | tr '\n' ' ')"
  fi
else
  bad "E3  pod envpod-sec never became Running"
fi

# --- E4: missing required ref blocks create (+ event); optional:true runs ------
new_pod envpod-missing podE4a.json '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"envpod-missing","namespace":"default"},"spec":{"containers":[{"name":"envpod","image":"'"$ENVPOD_REF"'","envFrom":[{"configMapRef":{"name":"no-such-configmap"}}]}]}}'
# Grace window: several sync cycles (2s each) must leave the pod non-Running.
BLOCKED=1
for _ in $(seq 1 24); do
  PH="$(curl -s "$BASE/api/v1/namespaces/default/pods/envpod-missing" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("status",{}).get("phase") or "")
except Exception: print("")' 2>/dev/null || true)"
  [[ "$PH" == "Running" ]] && { BLOCKED=0; break; }
  sleep 0.5
done
EV="$(wait_event envpod-missing CreateContainerConfigError || true)"
if [[ "$BLOCKED" == "1" && "$EV" == "EV" ]]; then
  ok "E4a missing required configMapRef: container never created + CreateContainerConfigError event"
else
  bad "E4a expected blocked pod + event (blocked=$BLOCKED event=${EV:-none})"
fi

new_pod envpod-opt podE4b.json '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"envpod-opt","namespace":"default"},"spec":{"containers":[{"name":"envpod","image":"'"$ENVPOD_REF"'","env":[{"name":"E4_MARK","value":"opt-runs"}],"envFrom":[{"configMapRef":{"name":"no-such-configmap","optional":true}}]}]}}'
if [[ "$(wait_pod_running envpod-opt)" == "RUN" ]]; then
  LOG="$(pod_log envpod-opt)"
  if grep -q '^ENV E4_MARK=opt-runs$' <<<"$LOG" && ! grep -q '^ENV E4_ONLY=' <<<"$LOG"; then
    ok "E4b optional:true missing ref: pod runs, missing source contributes nothing"
  else
    bad "E4b optional variant wrong: $(grep '^ENV E4_' <<<"$LOG" | tr '\n' ' ')"
  fi
else
  bad "E4b pod envpod-opt never became Running (optional ref must not block)"
fi

# --- teardown hygiene (G25C idiom): converge to zero sandboxes BEFORE TERM ----
for p in envpod-lit envpod-cm envpod-sec envpod-missing envpod-opt; do
  curl -s -o /dev/null -X DELETE "$BASE/api/v1/namespaces/default/pods/$p" || true
done
for _ in $(seq 1 120); do
  CNT="$(INIT_PRO_DATA_DIR="$DD/agent" "$BIN" crictl pods -o json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null || echo 99)"
  [[ "$CNT" == "0" ]] && break
  sleep 0.5
done

echo
echo "pod-env e2e: $PASS passed, $FAIL failed"
if [[ "$FAIL" -ne 0 ]]; then
  echo "pod-env e2e FAILED" >&2
  exit 1
fi
echo "ALL pod-env e2e checks passed"
