#!/usr/bin/env bash
# Sprint 18 / T4.2 Scope C (local deploy): boot a real single-node init-pro
# cluster on this machine — one `server` + one `agent` process over the
# bundled containerd (T4.1) with the Q27 airgap pause image — and keep it
# running until Ctrl-C. Interactive, long-lived twin of the golden G25
# recipe (scripts/golden-conformance.sh).
#
# Usage: scripts/local-up.sh
#   INIT_PRO_BIN=...       reuse a pre-built binary (default: build if missing)
#   INIT_PRO_API_PORT=...  fixed apiserver port (default: pick a free one)
#
# The embedded store is in-memory until T2.3 lands, so every boot is a fresh
# cluster and the data dir is a mktemp dir removed on exit.
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
DD="$(mktemp -d /tmp/init-pro-local.XXXXXX)"
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
  rm -rf "$DD"
  echo "local-up: bye."
}
trap cleanup EXIT
trap 'exit 130' INT TERM

echo "local-up: building the Q27 airgap pause image ..."
"$SCRIPT_DIR/build-pause-image.sh" "$PAUSE_TAR" "$PAUSE_REF" >/dev/null

echo "local-up: starting server on $BASE (data dir: $DD) ..."
# Sprint 18 / S4 (Q28): the NodePort service plane is on by default — the
# Router binds one listener per allocated nodePort, backed by live Endpoints.
"$BIN" server --data-dir "$DD/server" --bind-address 127.0.0.1 \
  --https-listen-port "$API_PORT" >"$DD/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 120); do grep -q "discovery listening" "$DD/server.log" 2>/dev/null && break; sleep 0.25; done
grep -q "discovery listening" "$DD/server.log" || { echo "error: server never came up (see $DD/server.log)" >&2; exit 1; }

echo "local-up: starting agent ..."
INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" "$BIN" agent --data-dir "$DD/agent" --token dev \
  --server "$BASE" --node-name local-node >"$DD/agent.log" 2>&1 &
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
cat <<FOLLOWUP

  API server  : $BASE
  Logs        : $DD/server.log   $DD/agent.log
  crictl      : INIT_PRO_DATA_DIR=$DD/agent $BIN crictl ps
  kubectl     : $DD/kubectl rollout status deployment/NAME --server $BASE

  Try a Deployment (pause pods, airgap image $PAUSE_REF):

    curl -s -X POST -H 'Content-Type: application/json' \\
      -d '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"demo","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"demo"}},"template":{"metadata":{"labels":{"app":"demo"}},"spec":{"containers":[{"name":"pause","image":"$PAUSE_REF"}]}}}}' \\
      $BASE/apis/apps/v1/namespaces/default/deployments

    curl -s $BASE/api/v1/namespaces/default/pods | python3 -m json.tool

  Ctrl-C to tear everything down.

FOLLOWUP
wait "$SERVER_PID"
