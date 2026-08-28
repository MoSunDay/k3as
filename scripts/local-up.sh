#!/usr/bin/env bash
# Sprint 18 / T4.2 Scope C (local deploy): boot a real single-node init-pro
# cluster on this machine — one `server` + one `agent` process over the
# bundled containerd (T4.1) with the Q27 airgap pause image — and keep it
# running until Ctrl-C. Interactive, long-lived twin of the golden G25
# recipe (scripts/golden-conformance.sh).
#
# Usage: scripts/local-up.sh
#   INIT_PRO_BIN=...        reuse a pre-built binary (default: build if missing)
#   INIT_PRO_API_PORT=...   fixed apiserver port (default: pick a free one)
#   LOCAL_UP_DATA_DIR=DIR   persistent data dir (T4.2 Scope C): keeps
#                           pods/logs across runs; default remains an
#                           ephemeral mktemp dir removed on exit.
#
# The datastore is rdb/RocksDB on disk since T2.3 (Q33): the server is started
# with `--datastore-endpoint rdb://$DD/server/state.rdb`, so the local
# cluster is RESTARTABLE within a run — data persists across server
# restarts. Since T4.3 Scope A (Q34) the server also allocates node
# podCIDRs from `--cluster-cidr 10.42.0.0/16` and the agent runs the
# embedded flanneld (`--flannel-backend vxlan`), so pods get real
# cluster-network IPs. The data dir is still a mktemp dir removed on exit unless
# LOCAL_UP_DATA_DIR opts into persistence (caller owns the tree then).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ "$(id -u)" == "0" ]] || { echo "error: local-up needs root (agent supervises containerd)" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }
command -v curl >/dev/null || { echo "error: curl is required" >&2; exit 1; }

if [[ ! -x "$BIN" ]]; then
  echo "local-up: $BIN missing; building (cargo build --workspace --locked) ..."
  (cd "$ROOT" && cargo build --workspace --locked)
fi
[[ -x "$BIN" ]] || { echo "error: still no binary at $BIN" >&2; exit 1; }

pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
    || echo $((20000 + RANDOM % 10000))
}

API_PORT="${INIT_PRO_API_PORT:-$(pick_port)}"
BASE="http://127.0.0.1:$API_PORT"
if [[ -n "${LOCAL_UP_DATA_DIR:-}" ]]; then
  # Persistent mode (T4.2 Scope C): caller owns the tree.
  DD="$LOCAL_UP_DATA_DIR"
  mkdir -p "$DD"
else
  DD="$(mktemp -d /tmp/init-pro-local.XXXXXX)"
fi
PAUSE_REF="init-pro.local/pause:0.1"
PAUSE_TAR="$DD/pause.tar"
SERVER_PID="" AGENT_PID=""

cleanup() {
  trap - EXIT INT TERM
  echo; echo "local-up: shutting down ..."
  [[ -n "$AGENT_PID" ]] && { kill -TERM "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; }
  [[ -n "$SERVER_PID" ]] && { kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; }
  # Running pods survive the agent's graceful drain (kubelet v1 does not
  # evict on shutdown): reap their shims and drop the leftover mounts so
  # the data dir can actually go away.
  pkill -f "$DD/agent/.*/containerd-shim" 2>/dev/null || true
  awk -v dd="$DD" '$2 ~ dd {print $2}' /proc/mounts | sort -r | while read -r m; do
    umount -l "$m" 2>/dev/null || true
  done
  sleep 0.5
  # Ephemeral runs own the dir and remove it; LOCAL_UP_DATA_DIR runs leave
  # the tree for the caller (pods/logs survive, Q31).
  if [[ -z "${LOCAL_UP_DATA_DIR:-}" ]]; then
    rm -rf "$DD"
  else
    echo "local-up: data kept at $DD (LOCAL_UP_DATA_DIR)"
  fi
  echo "local-up: bye."
}
trap cleanup EXIT
# Route fatal signals through the EXIT trap (bash skips it on untrapped signals).
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

echo "local-up: building the Q27 airgap pause image ..."
"$SCRIPT_DIR/build-pause-image.sh" "$PAUSE_TAR" "$PAUSE_REF" >/dev/null

echo "local-up: starting server on $BASE (data dir: $DD) ..."
# Sprint 18 / S4 (Q28): the NodePort service plane is on by default — the
# Router binds one listener per allocated nodePort, backed by live Endpoints.
# Sprint 19 / T2.3 (Q29): durable SQLite datastore under the data dir, so a
# server restart inside this run keeps the cluster state (Q33: rdb/RocksDB).
mkdir -p "$DD/server"
"$BIN" server --data-dir "$DD/server" --bind-address 127.0.0.1 \
  --datastore-endpoint "rdb://$DD/server/state.rdb" \
  --cluster-cidr 10.42.0.0/16 \
  --https-listen-port "$API_PORT" >"$DD/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 120); do grep -q "discovery listening" "$DD/server.log" 2>/dev/null && break; sleep 0.25; done
grep -q "discovery listening" "$DD/server.log" || { echo "error: server never came up (see $DD/server.log)" >&2; exit 1; }

echo "local-up: starting agent ..."
KUBELET_PORT="$(pick_port)"
INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" \
"$BIN" agent --data-dir "$DD/agent" --token dev \
  --server "$BASE" --node-name local-node \
  --node-ip 127.0.0.1 --kubelet-port "$KUBELET_PORT" \
  --flannel-backend vxlan >"$DD/agent.log" 2>&1 &
AGENT_PID=$!
for _ in $(seq 1 240); do grep -q "containerd healthy" "$DD/agent.log" 2>/dev/null && break; sleep 0.25; done
grep -q "containerd healthy" "$DD/agent.log" || { echo "error: containerd never healthy (see $DD/agent.log)" >&2; exit 1; }

echo "local-up: importing the pause image into the agent containerd ..."
INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$PAUSE_TAR" >/dev/null

# k3s-style boot gate: the node must report Ready before the cluster is usable.
NODE_READY=0
for _ in $(seq 1 240); do
  S="$(curl -s "$BASE/api/v1/nodes" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for n in items:
  conds={c.get("type"):c.get("status") for c in n.get("status",{}).get("conditions",[])}
  if conds.get("Ready")=="True": print("READY"); break' 2>/dev/null || true)"
  [[ "$S" == "READY" ]] && { NODE_READY=1; break; }
  sleep 0.5
done
[[ "$NODE_READY" == "1" ]] || { echo "error: node never became Ready (see $DD/agent.log)" >&2; exit 1; }
echo "local-up: node Ready. Single-node cluster is UP."

ln -sf "$BIN" "$DD/kubectl"

# The suggested workload is a REAL echo pod (airgap image): it serves HTTP
# on the podIP AND leaves stderr lines in its CRI log file, so the logs/exec
# data plane (T4.2 Scope C) is demonstrable immediately. Applied via plain
# REST curl + $DD/demo.sh (kubectl apply/get still speak argoproj.io only -
# T7.3 Scope A).
ECHO_TAR="$DD/demo-echo.tar"
if "$SCRIPT_DIR/build-echo-image.sh" "$ECHO_TAR" "init-pro.local/echo:0.1" >/dev/null 2>&1; then
  for _ in $(seq 1 20); do
    if INIT_PRO_DATA_DIR="$DD/agent" "$BIN" ctr -n k8s.io images import "$ECHO_TAR" >/dev/null 2>&1; then
      break
    fi
    sleep 0.5
  done
fi
cat > "$DD/demo.json" <<JSON
{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"demo","namespace":"default"},
 "spec":{"replicas":1,"selector":{"matchLabels":{"app":"demo"}},
   "template":{"metadata":{"labels":{"app":"demo"}},
     "spec":{"containers":[{"name":"echo","image":"init-pro.local/echo:0.1",
       "ports":[{"containerPort":8080}]}]}}}}
JSON

cat > "$DD/demo.sh" <<'SCRIPT'
#!/usr/bin/env bash
# Create the demo Deployment and wait for its pod to go Running+Ready,
# then print the T4.2 Scope C one-liners. Uses REST for create/wait --
# the bundled kubectl apply/get speak argoproj.io only (T7.3 Scope A).
set -euo pipefail
DD="$(cd "$(dirname "$0")" && pwd)"
BASE="${INIT_PRO_SERVER:?export INIT_PRO_SERVER first (see local-up banner)}"
K="$DD/kubectl"
curl -s -X POST "$BASE/apis/apps/v1/namespaces/default/deployments" \
  -H 'Content-Type: application/json' --data-binary @"$DD/demo.json" >/dev/null
POD=""
for _ in $(seq 1 240); do
  POD="$(curl -s "$BASE/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: items=[]
for p in items:
  st=p.get("status",{})
  conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
  if st.get("phase")=="Running" and conds.get("Ready")=="True":
    print(p["metadata"]["name"]); break' 2>/dev/null || true)"
  [[ -n "$POD" ]] && break
  sleep 0.5
done
[[ -n "$POD" ]] || { echo "demo pod never became Running+Ready"; exit 1; }
echo "demo pod ready: $POD"
echo "  $K logs $POD                                # relayed CRI log bytes"
echo "  $K logs -f $POD                             # client-side poll follow"
echo "  $K exec $POD -- /bin/wfstep hello           # rc 0 through crictl exec"
echo "  $K exec $POD -- /bin/wfstep --fail oops     # remote exit 1 propagates"
SCRIPT
chmod +x "$DD/demo.sh"

cat <<FOLLOWUP

  API server  : $BASE
  Kubelet     : $KUBELET_PORT (node-ip 127.0.0.1; logs/exec relay target)
  Logs        : $DD/server.log   $DD/agent.log
  crictl      : INIT_PRO_DATA_DIR=$DD/agent $BIN crictl ps
  kubectl     : export INIT_PRO_SERVER=$BASE   # then just run $DD/kubectl <verb> ...

  Try the echo-pod demo (create + wait handled for you):

    export INIT_PRO_SERVER=$BASE   # kubectl's --server env fallback
    $DD/demo.sh                    # creates the demo Deployment via REST, waits Ready

    then run the verbs it prints, e.g.:
    $DD/kubectl logs \$POD              # streamed CRI log bytes via the relay
    $DD/kubectl exec \$POD -- /bin/wfstep --fail oops   # remote exit 1 propagates

  Ctrl-C to tear everything down${LOCAL_UP_DATA_DIR:+ (data stays at $DD)}.

FOLLOWUP
wait "$SERVER_PID"
