#!/usr/bin/env bash
# T7.6 (Q62) FAKE-scope inference e2e: proves the llama.cpp serving
# lifecycle on the ordinary-object plane with NO real GPU, GGUF, or
# llama-server — INF1 (ordinary Deployment + NodePort Service, no
# Agent/CRD), INF2 (/health readiness + /v1/models + chat), INF3-fake
# (ordered SSE stream + clean [DONE]), GPU-fake evidence (CUDA_VISIBLE_DEVICES
# from the fake ledger injection), and INF5 (offline/fake: no network
# downloads — bogus image + imagePullPolicy: Never never pulls — and no
# model-aware Router configuration generated).
#
# The server binary is scripts/lib/fake-openai.py (python3 stdlib only),
# run INSIDE a Q60 host-direct pod: literal `init-pro.io/rootfs: /` over
# the builtin engine (host rootfs mode) + a Q61 whole-GPU request against
# a FAKE inventory (`--gpu-inventory fake:nvidia.com/gpu:1`), with the
# "GGUF" prestage shape mirrored by a read-only host-mount. This is
# fake-scope evidence ONLY: whole-T7.6 closure needs the recorded
# real-hardware harness (INF6) per Q62.
#
# Preconditions: PRE-BUILT binary with the T7.6 agent `--gpu-inventory`
# flag, real root (pivot_root/mount), curl + python3. Missing inputs =>
# "SKIP inference-fake-e2e:" + exit 0 (Q25 doctrine); a violated
# assertion is a hard FAIL.
#
# Usage: scripts/inference-fake-e2e.sh
#   INIT_PRO_BIN=...  reuse a specific binary (default target/debug/init-pro)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"
FAKE_OPENAI="$ROOT/scripts/lib/fake-openai.py"
MODEL_ALIAS="fake-model"
NODE="inference-node"

skip() { echo "SKIP inference-fake-e2e: $*"; exit 0; }
ok()   { echo "OK   $*"; }
fail() { echo "FAIL $*"; FAIL=$((FAIL+1)); }

[[ -x "$BIN" ]] || skip "no binary at $BIN (build first)"
[[ -f "$FAKE_OPENAI" ]] || skip "no fake OpenAI server at $FAKE_OPENAI"
command -v curl >/dev/null || skip "curl missing"
command -v python3 >/dev/null || skip "python3 missing"
[[ "$(id -u)" == "0" ]] || skip "needs real root (host-direct pivot_root/mount)"
# INF1 needs the T7.2 fake-inventory agent support; probe the help text
# (clap's --help exits 0 even with unknown args, so the exit code lies)
# before booting so an older binary SKIPs instead of false-failing.
"$BIN" agent --help 2>&1 | grep -q -- '--gpu-inventory' \
  || skip "binary lacks the T7.6 agent --gpu-inventory flag (INF1 fake inventory)"

pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}
stop_pid() {
  local pid="$1" i
  kill -TERM "$pid" 2>/dev/null || { wait "$pid" 2>/dev/null || true; return 0; }
  for i in $(seq 1 40); do kill -0 "$pid" 2>/dev/null || break; sleep 0.25; done
  kill -KILL "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

API_PORT="$(pick_port)"
REG_PORT="$(pick_port)"
KUBELET_PORT="$(pick_port)"
# Q52: builtin pods share the HOST netns, so the container port must dodge
# anything already squatting on the host (picked like every other port).
SRV_PORT="$(pick_port)"
# Host-direct exec does NO PATH resolution (nsrun execve takes argv[0]
# literally — same idiom as builtin-hostroot-e2e.sh's $BUSYBOX argv): both
# the shell and python3 are pinned to absolute host paths.
SH_BIN="/bin/sh"
PY3_BIN="$(command -v python3)"
[[ -x "$SH_BIN" ]] || skip "no /bin/sh on the host"
[[ -n "$PY3_BIN" && -x "$PY3_BIN" ]] || skip "no resolvable python3 on the host"
BASE="http://127.0.0.1:${API_PORT}"
REG="http://127.0.0.1:${REG_PORT}"
DD="$(mktemp -d /tmp/init-pro-inference-fake.XXXXXX)"
SRV_LOG="$DD/server.log"; AGENT_LOG="$DD/agent.log"
SRV_PID=""; AGENT_PID=""
PASS=0; FAIL=0

cleanup() {
  trap - EXIT
  [[ -n "${AGENT_PID:-}" ]] && { stop_pid "$AGENT_PID"; AGENT_PID=""; }
  [[ -n "${SRV_PID:-}" ]] && { stop_pid "$SRV_PID"; SRV_PID=""; }
  rm -rf "$DD" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# --- boot: server + builtin agent on the Q60/Q61 triple + fake inventory
"$BIN" server --data-dir "$DD/srv" --bind-address 127.0.0.1 \
  --https-listen-port "$API_PORT" \
  --registry-addr "127.0.0.1:${REG_PORT}" >"$SRV_LOG" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 100); do grep -q "discovery listening" "$SRV_LOG" && break; sleep 0.1; done
grep -q "discovery listening" "$SRV_LOG" \
  || { echo "error: server never came up"; tail -5 "$SRV_LOG" >&2; exit 1; }
"$BIN" agent --data-dir "$DD/agent" --token dev --server "$BASE" \
  --node-name "$NODE" --container-engine builtin --builtin-rootfs host \
  --kubelet-port "$KUBELET_PORT" --registry-addr "127.0.0.1:${REG_PORT}" \
  --gpu-inventory "fake:nvidia.com/gpu:1" >"$AGENT_LOG" 2>&1 &
AGENT_PID=$!
for _ in $(seq 1 300); do grep -q "kubelet started" "$AGENT_LOG" && break; sleep 0.1; done
grep -q "kubelet started" "$AGENT_LOG" \
  || { echo "error: builtin agent never came up"; tail -5 "$AGENT_LOG" >&2; exit 1; }
ok "cluster up (server + builtin host-rootfs agent, fake GPU inventory)"

# --- fake "GGUF" prestage: immutable local asset, exposed read-only (Q62)
MODELS_DIR="$DD/models"
mkdir -p "$MODELS_DIR"
printf 'fake-gguf-placeholder' > "$MODELS_DIR/${MODEL_ALIAS}.gguf"
chmod 444 "$MODELS_DIR/${MODEL_ALIAS}.gguf"

post() { # PATH FILE -> http code
  curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary "@$2" "$BASE$1"
}

# --- manifests (JSON-only wire, Q10) --------------------------------------
cat > "$DD/service.json" <<EOF
{
  "apiVersion": "v1", "kind": "Service",
  "metadata": {"name": "llama", "namespace": "default", "labels": {"app": "llama"}},
  "spec": {"type": "NodePort", "selector": {"app": "llama"},
           "ports": [{"name": "api", "port": 8080, "targetPort": ${SRV_PORT}, "protocol": "TCP"}]}
}
EOF
# NOTE: GPU quantities below MUST be JSON strings ("1", k8s wire
# convention) — kubelet's parse_requirements drops non-string rows.
cat > "$DD/deployment.json" <<EOF
{
  "apiVersion": "apps/v1", "kind": "Deployment",
  "metadata": {"name": "llama", "namespace": "default", "labels": {"app": "llama"}},
  "spec": {
    "replicas": 1,
    "strategy": {"type": "Recreate"},
    "selector": {"matchLabels": {"app": "llama"}},
    "template": {
      "metadata": {
        "labels": {"app": "llama"},
        "annotations": {
          "init-pro.io/rootfs": "/",
          "init-pro.io/host-mounts": "/models=${MODELS_DIR}:ro"
        }
      },
      "spec": {
        "nodeSelector": {"kubernetes.io/hostname": "${NODE}"},
        "containers": [{
          "name": "server",
          "image": "init-pro.local/llama:BOGUS-NO-PULL",
          "imagePullPolicy": "Never",
          "command": ["${SH_BIN}", "-c",
            "env | grep -E 'CUDA_VISIBLE_DEVICES|HIP_VISIBLE_DEVICES' || true; ls /models >/dev/null; exec ${PY3_BIN} ${FAKE_OPENAI} --port ${SRV_PORT} --model ${MODEL_ALIAS}"],
          "ports": [{"name": "http", "containerPort": ${SRV_PORT}}],
          "readinessProbe": {"httpGet": {"path": "/health", "port": ${SRV_PORT}},
                             "initialDelaySeconds": 0, "periodSeconds": 1,
                             "timeoutSeconds": 2, "failureThreshold": 3},
          "resources": {"requests": {"nvidia.com/gpu": "1"}, "limits": {"nvidia.com/gpu": "1"}}
        }]
      }
    }
  }
}
EOF

# --- IF1: ordinary objects; Service FIRST so the empty-Endpoints state is
# observable deterministically (no pods exist yet).
CODE="$(post /api/v1/namespaces/default/services "$DD/service.json")"
[[ "$CODE" == "201" ]] || { echo "error: service POST -> $CODE"; exit 1; }
SVC="$(curl -s "$BASE/api/v1/namespaces/default/services/llama")"
NODE_PORT="$(python3 -c 'import json,sys
s=json.load(sys.stdin); p=(s.get("spec",{}).get("ports") or [{}])[0]
print(p.get("nodePort") or "")' <<<"$SVC")"
if [[ "$NODE_PORT" =~ ^[0-9]+$ ]] && (( NODE_PORT >= 30000 && NODE_PORT <= 32767 )); then
  ok "IF1  NodePort Service allocated ${NODE_PORT} (range 30000-32767)"
  PASS=$((PASS+1))
else
  fail "IF1  no nodePort in range (got '${NODE_PORT:-none}')"
fi

EP="$(curl -s "$BASE/api/v1/namespaces/default/endpoints/llama")"
EP_ADDRS="$(python3 -c 'import json,sys
try: eps=json.load(sys.stdin).get("subsets") or []
except Exception: eps=[]
print(sum(len(s.get("addresses") or []) for s in eps))' <<<"$EP")"
NODEPORT_EMPTY="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$NODE_PORT/health" 2>/dev/null || true)"
if [[ "$EP_ADDRS" == "0" ]]; then
  ok "IF2  Endpoints empty before ready backends (nodePort /health -> ${NODEPORT_EMPTY:-conn-refused})"
  PASS=$((PASS+1))
else
  fail "IF2  Endpoints non-empty before any pod ($EP_ADDRS)"
fi

CODE="$(post /apis/apps/v1/namespaces/default/deployments "$DD/deployment.json")"
[[ "$CODE" == "201" ]] || { echo "error: deployment POST -> $CODE"; exit 1; }

# --- IF3: pod Running + Ready (readiness gated on /health via httpGet)
POD_NAME=""
for _ in $(seq 1 120); do
  READ="$(curl -s "$BASE/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for p in items:
  st=p.get("status",{})
  conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
  if st.get("phase")=="Running" and conds.get("Ready")=="True":
    print(p["metadata"]["name"]); break' 2>/dev/null || true)"
  [[ -n "$READ" ]] && { POD_NAME="$READ"; break; }
  sleep 0.5
done
if [[ -n "$POD_NAME" ]]; then
  ok "IF3  host-direct GPU pod Running+Ready (readiness /health): $POD_NAME"
  PASS=$((PASS+1))
else
  fail "IF3  pod never Running+Ready (agent: $AGENT_LOG)"
fi

# --- IF4: Endpoints populated + NodePort reachable once ready
EP_OK=0
for _ in $(seq 1 40); do
  EP="$(curl -s "$BASE/api/v1/namespaces/default/endpoints/llama")"
  EP_ADDRS="$(python3 -c 'import json,sys
try: eps=json.load(sys.stdin).get("subsets") or []
except Exception: eps=[]
ports=[q.get("port") for s in eps for q in (s.get("ports") or [])]
print(("1" if sum(len(s.get("addresses") or []) for s in eps)>=1 else "0"), ports)' <<<"$EP")"
  [[ "$EP_ADDRS" == "1 [$SRV_PORT]" ]] && { EP_OK=1; break; }
  sleep 0.5
done
if [[ "$EP_OK" -eq 1 ]]; then
  ok "IF4  Endpoints carry the ready pod on port $SRV_PORT"
  PASS=$((PASS+1))
else
  fail "IF4  endpoints never ready (got: ${EP_ADDRS:-none})"
fi

SVC_ADDR="http://127.0.0.1:$NODE_PORT"
HEALTH=""
for _ in $(seq 1 60); do
  HEALTH="$(curl -s --max-time 3 "$SVC_ADDR/health" 2>/dev/null || true)"
  grep -q '"status": *"ok"' <<<"$HEALTH" && break
  HEALTH=""
  sleep 0.5
done
if [[ -n "$HEALTH" ]]; then
  ok "IF4  NodePort /health reachable when ready: $HEALTH"
  PASS=$((PASS+1))
else
  fail "IF4  NodePort /health never ok ($SVC_ADDR)"
fi

# --- IF5: INF2 API surface through the NodePort
MODELS="$(curl -s --max-time 5 "$SVC_ADDR/v1/models" 2>/dev/null || true)"
if grep -q "\"$MODEL_ALIAS\"" <<<"$MODELS"; then
  ok "IF5  GET /v1/models advertises '$MODEL_ALIAS'"
  PASS=$((PASS+1))
else
  fail "IF5  /v1/models missing alias (got: $MODELS)"
fi
CHAT="$(curl -s --max-time 5 -X POST -H 'Content-Type: application/json' \
  --data '{"messages":[{"role":"user","content":"ping fake"}]}' \
  "$SVC_ADDR/v1/chat/completions" 2>/dev/null || true)"
if grep -q 'echo:ping fake' <<<"$CHAT" && grep -q '"finish_reason": *"stop"' <<<"$CHAT"; then
  ok "IF5  chat completions (non-stream JSON) round-trip"
  PASS=$((PASS+1))
else
  fail "IF5  non-stream chat wrong (got: $CHAT)"
fi

# --- IF6: INF3 streaming — ordered SSE chunks terminated by [DONE]
STREAM="$(curl -s -N --max-time 10 -X POST -H 'Content-Type: application/json' \
  --data '{"stream":true,"messages":[{"role":"user","content":"a b"}]}' \
  "$SVC_ADDR/v1/chat/completions" 2>/dev/null || true)"
# Reassemble the delta contents (the reply is split one word per chunk —
# the literal text never appears on a single line) + assert [DONE] is the
# terminal event.
STREAM_CHECK="$(python3 -c 'import json,sys
out=[]; done_last=False; ok=True
for line in sys.stdin:
    line=line.strip()
    if not line.startswith("data: "): continue
    body=line[6:]
    if body == "[DONE]":
        done_last=True; continue
    if done_last: ok=False  # events after [DONE]
    try: chunk=json.loads(body)
    except Exception: ok=False; continue
    if chunk.get("object") != "chat.completion.chunk": ok=False
    for ch in chunk.get("choices") or []:
        d=ch.get("delta") or {}
        if isinstance(d.get("content"), str): out.append(d["content"])
joined="".join(out)
print("OK" if ok and done_last and joined.startswith("echo:a") and "b" in joined else "BAD:%s"%joined)' <<<"$STREAM" 2>/dev/null || true)"
CHUNKS="$(grep -c '^data: ' <<<"$STREAM" || true)"
if [[ "$STREAM_CHECK" == "OK" && "$CHUNKS" -ge 3 ]] \
  && [[ "$(tail -1 <<<"$STREAM")" == "data: [DONE]" ]]; then
  ok "IF6  streaming SSE: $CHUNKS data: events, ordered deltas, clean [DONE]"
  PASS=$((PASS+1))
else
  fail "IF6  stream shape wrong ($STREAM_CHECK; chunks=$CHUNKS; $(head -c 200 <<<"$STREAM"))"
fi

# --- IF7: GPU fake evidence — the ledger-injected device env in the logs
LOGS=""
if [[ -n "$POD_NAME" ]]; then
  for _ in $(seq 1 20); do
    LOGS="$(curl -s --max-time 8 "$BASE/api/v1/namespaces/default/pods/$POD_NAME/log?container=server" 2>/dev/null || true)"
    grep -q 'CUDA_VISIBLE_DEVICES' <<<"$LOGS" && break
    sleep 0.5
  done
fi
if grep -Eq 'CUDA_VISIBLE_DEVICES=[0-9]+' <<<"$LOGS"; then
  ok "IF7  fake ledger injected CUDA_VISIBLE_DEVICES (fake-scope GPU evidence)"
  PASS=$((PASS+1))
else
  fail "IF7  no CUDA_VISIBLE_DEVICES in pod logs (fake inventory env injection)"
fi

# --- IF8: INF5 — no model-aware Router configuration was generated: the
# Router compiles routes ONLY from Ingress (Q32); TrafficSplit (T8.2)
# stays absent too. Plus: no image pull ever ran (bogus image + Never).
ING="$(curl -s "$BASE/apis/networking.k8s.io/v1/namespaces/default/ingresses" | python3 -c 'import json,sys
try: print(len(json.load(sys.stdin).get("items") or []))
except Exception: print("ERR")' 2>/dev/null || true)"
TS="$(curl -s "$BASE/apis/init-pro.io/v1/namespaces/default/trafficsplits" | python3 -c 'import json,sys
try: print(len(json.load(sys.stdin).get("items") or []))
except Exception: print("ERR")' 2>/dev/null || true)"
if [[ "$ING" == "0" && "$TS" == "0" ]]; then
  ok "IF8  zero Ingress / zero TrafficSplit — no model-aware routing generated"
  PASS=$((PASS+1))
else
  fail "IF8  model-aware routing leaked (ingresses=$ING trafficsplits=$TS)"
fi
# A pull attempt names the bogus reference; anything else (e.g. the
# host-direct "no image action" path) is the offline-correct outcome.
if grep -Eq '(pull|Pull|PULL)[^\n]*(BOGUS-NO-PULL|init-pro.local/llama)' "$AGENT_LOG"; then
  fail "IF8  the bogus image was pulled (offline rule broken)"
else
  ok "IF8  no image pull: bogus image + imagePullPolicy Never never fetched"
  PASS=$((PASS+1))
fi

echo "----"
echo "inference-fake-e2e: PASS=$PASS FAIL=$FAIL (fake scope: INF1/2/3/5, Q62)"
[[ "$FAIL" -eq 0 ]] || exit 1
