#!/usr/bin/env bash
# T0.6 golden conformance: the immutable baseline of k3s/k8s wire-level
# behaviors that every later TODO must keep green. Boots a real init-pro
# server and diffs its responses against committed golden fixtures in golden/.
#
# Today = the EMPTY-CLUSTER baseline: discovery (T1.2a) + CRUD/watch over the
# embedded store (T1.2b), extended by the T3.1a controller acceptance (G17:
# Deployment scale converge + Endpoints membership) and the T3.1b suite
# (G18-G21: rolling update + kubectl rollout status, StatefulSet ordinal
# identity + PVCs, DaemonSet per-Node scheduling, GC cascade + namespace
# drain). The discovery contract is
# byte-stable; CRUD cases
# collection endpoints exist yet. As CRUD/watch/storage/scheduling layers land
# (T1.2, T2.x, T3.x), their golden cases are appended below — each TODO tags
# which cases it must keep green (plan/00-foundation.md T0.6, Q2 merge gate).
#
# Later suites appended below: storage semantics G02-G11 (T2.1/T2.2),
# control plane G12-G23 (T3.1/T3.2), and the node layer G24-G25
# (T4.1 containerd supervision; T4.2 Scope A kubelet drives Deployment
# pods to Running+Ready over real containerd with the Q27 airgap pause
# image; G24/G25 SKIP without the vendor bundle, G25 also needs cc).
#
# Volatility: the only non-deterministic field is APIVersions.serverAddress
# (the bind host:port); it is normalized to the token @@PORT@@ before diffing.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"
GOLDEN="$ROOT/golden"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl >/dev/null || { echo "error: curl is required" >&2; exit 1; }
command -v diff >/dev/null || { echo "error: diff is required" >&2; exit 1; }

ok()  { echo "OK   $*"; }

# Pick a free loopback port (avoids clashing with a host apiserver / prior run).
pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
    || echo $((20000 + RANDOM % 10000))
}
PORT="$(pick_port)"

DD="$(mktemp -d)"
LOG="$(mktemp)"
"$BIN" server --data-dir "$DD" --bind-address 127.0.0.1 --https-listen-port "$PORT" >"$LOG" 2>&1 &
SRV=$!
SRV2=""; STUB_PID=""
cleanup() {
  kill "$SRV" 2>/dev/null || true; wait "$SRV" 2>/dev/null || true
  [[ -n "$SRV2" ]] && { kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; }
  [[ -n "$STUB_PID" ]] && kill "$STUB_PID" 2>/dev/null || true
  [[ -n "${AGENT_PID:-}" ]] && { kill "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; }
  rm -rf "$DD" "$LOG"
}
trap cleanup EXIT

for _ in $(seq 1 60); do grep -q "discovery listening" "$LOG" && break; sleep 0.1; done
grep -q "discovery listening" "$LOG" || { echo "error: server never reported listening"; cat "$LOG" >&2; exit 1; }

BASE="http://127.0.0.1:${PORT}"
PASS=0
FAIL=0

# Compare a live endpoint's body to its golden fixture.
#   $1 id   $2 path   $3 golden-file   $4 normalize: "serveraddr" | (else none)
check_body() {
  local id="$1" path="$2" golden_file="$3" norm="${4:-no}"
  local live
  live="$(curl -fsS "$BASE$path")" || { echo "FAIL $id  $path  (fetch failed)"; FAIL=$((FAIL+1)); return; }
  if [[ "$norm" == "serveraddr" ]]; then
    # Normalize the volatile bind address to the committed token.
    live="${live//127.0.0.1:$PORT/127.0.0.1:@@PORT@@}"
  fi
  local expected; expected="$(cat "$golden_file")"
  if [[ "$live" == "$expected" ]]; then
    ok "$id  $path"
    PASS=$((PASS+1))
  else
    echo "FAIL $id  $path  (body mismatch)"
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$live") | sed 's/^/    /' >&2
    FAIL=$((FAIL+1))
  fi
}

# Assert an endpoint returns an exact HTTP status.
check_status() {
  local id="$1" path="$2" want="$3" desc="$4"
  local code; code="$(curl -s -o /dev/null -w '%{http_code}' "$BASE$path")"
  if [[ "$code" == "$want" ]]; then
    ok "$id  $path -> $code  ($desc)"
    PASS=$((PASS+1))
  else
    echo "FAIL $id  $path -> $code (expected $want)  ($desc)"
    FAIL=$((FAIL+1))
  fi
}

# Assert an arbitrary HTTP method returns an exact status, optionally with a
# request body + content-type. Used for CRUD round-trips introduced by T1.2b.
#   $1 id   $2 method   $3 path   $4 want   $5 desc   [$6 content-type $7 body-file]
check_method() {
  local id="$1" method="$2" path="$3" want="$4" desc="$5" ct="${6:-}" body_file="${7:-}" max_time="${8:-}"
  local code extra=()
  [[ -n "$max_time" ]] && extra+=(--max-time "$max_time")
  # Suppress curl's non-zero exit (e.g. 28 on watch-stream timeout) so we
  # still get the %{http_code} that was received before the body stalled.
  if [[ -n "$body_file" ]]; then
    code="$(curl -s -o /dev/null -w '%{http_code}' "${extra[@]}" -X "$method" -H "Content-Type: $ct" --data-binary "@$body_file" "$BASE$path" 2>/dev/null || true)"
  else
    code="$(curl -s -o /dev/null -w '%{http_code}' "${extra[@]}" -X "$method" "$BASE$path" 2>/dev/null || true)"
  fi
  if [[ "$code" == "$want" ]]; then
    ok "$id  $method $path -> $code  ($desc)"
    PASS=$((PASS+1))
  else
    echo "FAIL $id  $method $path -> $code (expected $want)  ($desc)"
    FAIL=$((FAIL+1))
  fi
}

echo "## golden conformance — empty-cluster baseline + controller loops (port $PORT)"

# --- discovery contract (byte-stable) ---
check_body G01 "/api"                 "$GOLDEN/discovery-api.json"        serveraddr
check_body G02 "/apis"                "$GOLDEN/discovery-apis.json"
check_body G03 "/api/v1"              "$GOLDEN/discovery-core-v1.json"
check_body G04 "/apis/init-pro.io/v1" "$GOLDEN/discovery-initpro-v1.json"

# --- collection endpoints now exist (T1.2b) ---
check_status G05 "/apis/fabricated.io/v9beta1"           "404" "unknown group/version"
check_status G06 "/api/v1/pods"                          "200" "empty pods collection list (T1.2b)"

# --- CRUD round-trip over the embedded store (T1.2b) ---
TMP_CM="$(mktemp)"
printf '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"golden-cm","namespace":"default"},"data":{"k":"v"}}' > "$TMP_CM"
check_method G07 "POST"   "/api/v1/namespaces/default/configmaps"              "201" "create ConfigMap"                "application/json" "$TMP_CM"
check_method G08 "GET"    "/api/v1/namespaces/default/configmaps/golden-cm"    "200" "get ConfigMap"
check_method G09 "GET"    "/api/v1/namespaces/default/configmaps"              "200" "list ConfigMaps (1 item)"
check_method G10 "DELETE" "/api/v1/namespaces/default/configmaps/golden-cm"    "200" "delete ConfigMap"
check_method G11 "GET"    "/api/v1/namespaces/default/configmaps/golden-cm"    "404" "deleted ConfigMap is gone"
check_method G12 "GET"    "/api/v1/namespaces/default/configmaps?watch=1"      "200" "watch stream opens (T1.2b)" "" "" "2"
rm -f "$TMP_CM"

# --- Server-Side Apply over the embedded store (T1.2c) ---
TMP_APPLY="$(mktemp)"
printf '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"golden-apply-cm","namespace":"default"},"data":{"k":"v"}}' > "$TMP_APPLY"
check_method G13 "PATCH" "/api/v1/namespaces/default/configmaps/golden-apply-cm?fieldManager=golden-test" "201" "SSA apply creates ConfigMap (T1.2c)"  "application/apply-patch+yaml" "$TMP_APPLY"
check_method G14 "PATCH" "/api/v1/namespaces/default/configmaps/golden-apply-cm?fieldManager=golden-test" "200" "SSA apply updates ConfigMap (T1.2c)" "application/apply-patch+yaml" "$TMP_APPLY"
rm -f "$TMP_APPLY"

# --- watch historical replay (T2.2) ---
# G15: `?watch=1&resourceVersion=0` must replay retained history (the ADDED
# for the just-created ConfigMap) before any live events -- the informer
# disconnect/reconnect contract. curl is bounded by --max-time (exit 28 is
# expected once the replay is drained and the stream idles).
printf '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"golden-watch-cm","namespace":"default"},"data":{"k":"v"}}' > "$TMP_CM"
G15_CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_CM" "$BASE/api/v1/namespaces/default/configmaps")"
G15_BODY="$(curl -sN --max-time 3 "$BASE/api/v1/namespaces/default/configmaps?watch=1&resourceVersion=0" 2>/dev/null || true)"
if [[ "$G15_CODE" == "201" ]] \
   && printf '%s' "$G15_BODY" | grep -q '"type":"ADDED"' \
   && printf '%s' "$G15_BODY" | grep -q 'golden-watch-cm'; then
  ok "G15  GET /api/v1/namespaces/default/configmaps?watch=1&resourceVersion=0 -> replayed ADDED  (watch history replay, T2.2)"
  PASS=$((PASS+1))
else
  echo "FAIL G15  watch resourceVersion=0 did not replay history (create status $G15_CODE); body: $G15_BODY"
  FAIL=$((FAIL+1))
fi
rm -f "$TMP_CM"

# --- controllers wired into the server (T3.1a) ---
# G16: the apps/v1 group the controllers serve is part of the byte-stable
# discovery contract (Deployment/ReplicaSet driven by T3.1a; StatefulSet/
# DaemonSet schema-only until T3.1b).
check_body G16 "/apis/apps/v1" "$GOLDEN/discovery-apps-v1.json"

# G17: T3.1a acceptance -- a real Deployment scale 1->3->1 must converge
# (Deployment -> ReplicaSet -> Pods) and a selector Service's Endpoints must
# reflect pod membership at each scale. StatefulSet/DaemonSet/GC/rollout
# status are T3.1b and get their own cases later.
TMP_DEP3="$(mktemp)"
TMP_DEP1="$(mktemp)"
TMP_SVC="$(mktemp)"
printf '%s' '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"golden-dep","namespace":"default"},"spec":{"replicas":3,"selector":{"matchLabels":{"app":"golden-web"}},"template":{"metadata":{"labels":{"app":"golden-web"}},"spec":{"containers":[{"name":"web","image":"nginx:1.29","ports":[{"containerPort":80}]}]}}}}' > "$TMP_DEP3"
# Same object, scaled to 1 (full-object PUT, kubectl-style).
sed 's/"replicas":3/"replicas":1/' "$TMP_DEP3" > "$TMP_DEP1"
printf '%s' '{"apiVersion":"v1","kind":"Service","metadata":{"name":"golden-dep-svc","namespace":"default"},"spec":{"selector":{"app":"golden-web"},"ports":[{"port":80}]}}' > "$TMP_SVC"
DEP_PATH="/apis/apps/v1/namespaces/default/deployments/golden-dep"
PODS_PATH="/api/v1/namespaces/default/pods"
EPS_PATH="/api/v1/namespaces/default/endpoints/golden-dep-svc"

# Count live pods owned by the deployment. NOTE: pod items carry an
# ownerReference whose name also matches "golden-dep-*", so counting names
# over-counts; each pod item repeats its own "kind":"Pod" exactly once
# (the list envelope is "PodList", which does not match). `|| true` keeps
# grep's no-match exit (empty list) from tripping `set -o pipefail`.
pod_count() { curl -s "$BASE$PODS_PATH" | grep -o '"kind":"Pod"' | wc -l || true; }
# Endpoint subset addresses: one "ip" field per address entry.
ep_addr_count() { curl -s "$BASE$EPS_PATH" | grep -o '"ip"' | wc -l || true; }

# 1. Create the Deployment (replicas=3) + the selector Service.
G17_CREATE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_DEP3" "$BASE/apis/apps/v1/namespaces/default/deployments")"
G17_SVC="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_SVC" "$BASE/api/v1/namespaces/default/services")"

# 2. Poll for convergence: status.replicas==3 AND 3 pods (max ~15s).
G17_SCALE3_PODS=0
for _ in $(seq 1 60); do
  D="$(curl -s "$BASE$DEP_PATH")"
  N="$(pod_count)"
  if printf '%s' "$D" | grep -q '"replicas":3' && [[ "$N" -eq 3 ]]; then
    G17_SCALE3_PODS=1; break
  fi
  sleep 0.25
done

# 3/4. Endpoints must carry all 3 selector-matched pods as addresses.
G17_EPS3=0
for _ in $(seq 1 60); do
  if [[ "$(ep_addr_count)" -eq 3 ]]; then G17_EPS3=1; break; fi
  sleep 0.25
done

# 5. Scale down 3->1 (PUT full object) and wait for pods + endpoints to match.
G17_SCALE1_CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "Content-Type: application/json" --data-binary "@$TMP_DEP1" "$BASE$DEP_PATH")"
G17_SCALE1_PODS=0
for _ in $(seq 1 60); do
  D="$(curl -s "$BASE$DEP_PATH")"
  N="$(pod_count)"
  if printf '%s' "$D" | grep -q '"replicas":1' && [[ "$N" -eq 1 ]]; then
    G17_SCALE1_PODS=1; break
  fi
  sleep 0.25
done
G17_EPS1=0
for _ in $(seq 1 60); do
  if [[ "$(ep_addr_count)" -eq 1 ]]; then G17_EPS1=1; break; fi
  sleep 0.25
done

if [[ "$G17_CREATE" == "201" && "$G17_SVC" == "201" && "$G17_SCALE3_PODS" -eq 1 && "$G17_EPS3" -eq 1 \
      && "$G17_SCALE1_CODE" == "200" && "$G17_SCALE1_PODS" -eq 1 && "$G17_EPS1" -eq 1 ]]; then
  ok "G17  deployment golden-dep scale 3->1 converge + endpoints membership  (controllers, T3.1a)"
  PASS=$((PASS+1))
else
  echo "FAIL G17  controller convergence (create=$G17_CREATE svc=$G17_SVC scale3pods=$G17_SCALE3_PODS eps3=$G17_EPS3 put=$G17_SCALE1_CODE scale1pods=$G17_SCALE1_PODS eps1=$G17_EPS1)"
  FAIL=$((FAIL+1))
fi
rm -f "$TMP_DEP3" "$TMP_DEP1" "$TMP_SVC"

# --- T3.1b: rolling update + rollout status / StatefulSet / DaemonSet / GC ---
# The controller-manager acceptance of T3.1b. G18 drives the REAL kubectl
# surface (argv[0] multicall symlink) against the live server; G19-G21 poll
# object convergence through the REST surface.

# A poll helper: run BODY_CMD until GREP finds PATTERN (or ~15s timeout).
wait_for() {
  local pattern="$1" body_cmd="$2"
  for _ in $(seq 1 60); do
    if eval "$body_cmd" | grep -q "$pattern"; then return 0; fi
    sleep 0.25
  done
  return 1
}

# kubectl via argv[0] multicall symlink (T0.1 contract; the alias now has a
# real implementation, T3.1b).
KCTL_DIR="$(mktemp -d)"
ln -sf "$(readlink -f "$BIN")" "$KCTL_DIR/kubectl"

# G18: rolling update + `kubectl rollout status` exit codes.
# 1. Fresh Deployment (nginx:1.28) converges -> rollout status exit 0.
# 2. Image roll to nginx:1.29 -> status.conditions gains Progressing
#    reason NewReplicaSetAvailable + availableReplicas==2 -> exit 0 again.
# 3. `rollout status` of a missing Deployment -> exit 1 (NotFound).
TMP_G18="$(mktemp)"
printf '%s' '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"golden-roll"},"spec":{"replicas":2,"selector":{"matchLabels":{"app":"golden-roll"}},"template":{"metadata":{"labels":{"app":"golden-roll"}},"spec":{"containers":[{"name":"web","image":"nginx:1.28"}]}}}}' > "$TMP_G18"
G18_CREATE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G18" "$BASE/apis/apps/v1/namespaces/default/deployments")"
G18_PATH="/apis/apps/v1/namespaces/default/deployments/golden-roll"
G18_RC1=0; G18_OUT="$("$KCTL_DIR/kubectl" rollout status deployment/golden-roll --server "$BASE" 2>&1)" || G18_RC1=$?
wait_for 'NewReplicaSetAvailable' "curl -s $BASE$G18_PATH" \
  || true # belt: the kubectl exit code below is the real assertion
sed 's/nginx:1.28/nginx:1.29/' "$TMP_G18" > "$TMP_G18.new"
curl -s -o /dev/null -X PUT -H "Content-Type: application/json" --data-binary "@$TMP_G18.new" "$BASE$G18_PATH"
G18_RC2=1
for _ in $(seq 1 40); do
  if curl -s "$BASE$G18_PATH" | grep -q '"availableReplicas":2' \
     && curl -s "$BASE$G18_PATH" | grep -q 'NewReplicaSetAvailable'; then G18_RC2=0; break; fi
  sleep 0.25
done
G18_RC3=0; "$KCTL_DIR/kubectl" rollout status deployment/golden-roll --server "$BASE" >/dev/null 2>&1 || G18_RC3=$?
G18_RC4=0; "$KCTL_DIR/kubectl" rollout status deployment/golden-missing --server "$BASE" >/dev/null 2>&1 || G18_RC4=$?
if [[ "$G18_CREATE" == "201" && $G18_RC1 -eq 0 && $G18_RC2 -eq 0 && $G18_RC3 -eq 0 && $G18_RC4 -eq 1 ]] \
   && printf '%s' "$G18_OUT" | grep -q 'successfully rolled out'; then
  ok "G18  rolling update converge + kubectl rollout status exit codes  (controllers, T3.1b)"
  PASS=$((PASS+1))
else
  echo "FAIL G18  rollout (create=$G18_CREATE rc1=$G18_RC1 rc2=$G18_RC2 rc3=$G18_RC3 rc4=$G18_RC4 out=$G18_OUT)"
  FAIL=$((FAIL+1))
fi
rm -f "$TMP_G18" "$TMP_G18.new"

# G19: StatefulSet ordered identity + PVC retention. Scale 3 -> web-0/1/2 +
# one PVC per claim template per ordinal; scale to 1 removes high ordinals
# but NEVER the PVCs.
TMP_G19="$(mktemp)"
printf '%s' '{"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"golden-web"},"spec":{"replicas":3,"serviceName":"golden-web","selector":{"matchLabels":{"app":"golden-web"}},"template":{"metadata":{"labels":{"app":"golden-web"}},"spec":{"containers":[{"name":"web","image":"nginx:1.29"}]}},"volumeClaimTemplates":[{"metadata":{"name":"data"},"spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Gi"}}}}]}}' > "$TMP_G19"
G19_CREATE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G19" "$BASE/apis/apps/v1/namespaces/default/statefulsets")"
STS_PATH="/apis/apps/v1/namespaces/default/statefulsets/golden-web"
G19_UP=0
for _ in $(seq 1 60); do
  P0="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/namespaces/default/pods/golden-web-0")"
  P1="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/namespaces/default/pods/golden-web-1")"
  P2="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/namespaces/default/pods/golden-web-2")"
  PVC="$(curl -s "$BASE/api/v1/namespaces/default/persistentvolumeclaims" | python3 -c "import json,sys; print(len(json.load(sys.stdin).get('items',[])))" 2>/dev/null || echo 99)"
  if [[ "$P0" == "200" && "$P1" == "200" && "$P2" == "200" && "$PVC" -eq 3 ]]; then G19_UP=1; break; fi
  sleep 0.25
done
# Scale to 1 (full-object PUT): pods -1/-2 go, PVCs stay.
sed 's/"replicas":3/"replicas":1/' "$TMP_G19" > "$TMP_G19.1"
G19_PUT="$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "Content-Type: application/json" --data-binary "@$TMP_G19.1" "$BASE$STS_PATH")"
G19_DOWN=0
for _ in $(seq 1 60); do
  P0="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/namespaces/default/pods/golden-web-0")"
  P1="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/namespaces/default/pods/golden-web-1")"
  P2="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/namespaces/default/pods/golden-web-2")"
  PVC="$(curl -s "$BASE/api/v1/namespaces/default/persistentvolumeclaims" | python3 -c "import json,sys; print(len(json.load(sys.stdin).get('items',[])))" 2>/dev/null || echo 99)"
  if [[ "$P0" == "200" && "$P1" == "404" && "$P2" == "404" && "$PVC" -eq 3 ]]; then G19_DOWN=1; break; fi
  sleep 0.25
done
G19_STATUS=0
curl -s "$BASE$STS_PATH" | grep -q '"replicas":1' && curl -s "$BASE$STS_PATH" | grep -q '"readyReplicas":1' && G19_STATUS=1
if [[ "$G19_CREATE" == "201" && $G19_UP -eq 1 && "$G19_PUT" == "200" && $G19_DOWN -eq 1 && $G19_STATUS -eq 1 ]]; then
  ok "G19  statefulset ordinal scale 3->1 + per-ordinal PVCs retained  (controllers, T3.1b)"
  PASS=$((PASS+1))
else
  echo "FAIL G19  statefulset (create=$G19_CREATE up=$G19_UP put=$G19_PUT down=$G19_DOWN status=$G19_STATUS)"
  FAIL=$((FAIL+1))
fi
rm -f "$TMP_G19" "$TMP_G19.1"

# G20: DaemonSet follows Node lifecycle: 1 matching Node -> 1 pod (nodeName
# pinned); second Node -> 2 pods; Node deleted -> back to 1.
TMP_N1="$(mktemp)"; TMP_N2="$(mktemp)"; TMP_DS="$(mktemp)"
printf '%s' '{"apiVersion":"v1","kind":"Node","metadata":{"name":"golden-n1","labels":{"topology":"golden"}}}' > "$TMP_N1"
printf '%s' '{"apiVersion":"v1","kind":"Node","metadata":{"name":"golden-n2","labels":{"topology":"golden"}}}' > "$TMP_N2"
printf '%s' '{"apiVersion":"apps/v1","kind":"DaemonSet","metadata":{"name":"golden-ds"},"spec":{"selector":{"matchLabels":{"app":"golden-ds"}},"template":{"metadata":{"labels":{"app":"golden-ds"}},"spec":{"nodeSelector":{"topology":"golden"},"containers":[{"name":"agent","image":"nginx:1.29"}]}}}}' > "$TMP_DS"
G20_N1="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_N1" "$BASE/api/v1/nodes")"
G20_DS="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_DS" "$BASE/apis/apps/v1/namespaces/default/daemonsets")"
DS_PATH="/apis/apps/v1/namespaces/default/daemonsets/golden-ds"
ds_on_node() { curl -s "$BASE/api/v1/namespaces/default/pods" | python3 -c "import json,sys; print(sum(1 for p in json.load(sys.stdin)['items'] if p.get('spec',{}).get('nodeName')=='$1' and any(o.get('kind')=='DaemonSet' and o.get('name')=='golden-ds' for o in p.get('metadata',{}).get('ownerReferences',[]))))" 2>/dev/null || echo 0; }
G20_ONE=0
for _ in $(seq 1 60); do
  if [[ "$(ds_on_node golden-n1)" -eq 1 ]] && curl -s "$BASE$DS_PATH" | grep -q '"desiredNumberScheduled":1'; then G20_ONE=1; break; fi
  sleep 0.25
done
G20_N2="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_N2" "$BASE/api/v1/nodes")"
G20_TWO=0
for _ in $(seq 1 60); do
  if [[ "$(ds_on_node golden-n1)" -eq 1 && "$(ds_on_node golden-n2)" -eq 1 ]] \
     && curl -s "$BASE$DS_PATH" | grep -q '"desiredNumberScheduled":2'; then G20_TWO=1; break; fi
  sleep 0.25
done
G20_DEL="$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/v1/nodes/golden-n2")"
G20_BACK=0
for _ in $(seq 1 60); do
  if [[ "$(ds_on_node golden-n2)" -eq 0 ]] && curl -s "$BASE$DS_PATH" | grep -q '"desiredNumberScheduled":1'; then G20_BACK=1; break; fi
  sleep 0.25
done
if [[ "$G20_N1" == "201" && "$G20_DS" == "201" && $G20_ONE -eq 1 && "$G20_N2" == "201" && $G20_TWO -eq 1 && "$G20_DEL" == "200" && $G20_BACK -eq 1 ]]; then
  ok "G20  daemonset one-pod-per-matching-node + node lifecycle  (controllers, T3.1b)"
  PASS=$((PASS+1))
else
  echo "FAIL G20  daemonset (n1=$G20_N1 ds=$G20_DS one=$G20_ONE n2=$G20_N2 two=$G20_TWO del=$G20_DEL back=$G20_BACK)"
  FAIL=$((FAIL+1))
fi
rm -f "$TMP_N1" "$TMP_N2" "$TMP_DS"

# G21: GC cascade + namespace drain. Deleting the Deployment cascades to its
# ReplicaSet + pods; deleting a Namespace drains its contents then itself
# (finalizer "kubernetes" injected at create; namespace controller drains +
# performs the terminal delete, Q20).
TMP_G21D="$(mktemp)"; TMP_G21NS="$(mktemp)"; TMP_G21CM="$(mktemp)"
printf '%s' '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"golden-casc"},"spec":{"replicas":2,"selector":{"matchLabels":{"app":"golden-casc"}},"template":{"metadata":{"labels":{"app":"golden-casc"}},"spec":{"containers":[{"name":"web","image":"nginx:1.29"}]}}}}' > "$TMP_G21D"
printf '%s' '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"golden-ns"}}' > "$TMP_G21NS"
printf '%s' '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"golden-cm","namespace":"golden-ns"},"data":{"k":"v"}}' > "$TMP_G21CM"
G21_CREATE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G21D" "$BASE/apis/apps/v1/namespaces/default/deployments")"
wait_for '"replicas":2' "curl -s $BASE/apis/apps/v1/namespaces/default/deployments/golden-casc" || true
G21_DEL="$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/apis/apps/v1/namespaces/default/deployments/golden-casc")"
G21_CASC=0
for _ in $(seq 1 60); do
  RS="$(curl -s "$BASE/apis/apps/v1/namespaces/default/replicasets" | python3 -c "import json,sys; print(sum(1 for i in json.load(sys.stdin).get('items',[]) if i['metadata']['name'].startswith('golden-casc')))" 2>/dev/null || echo 99)"
  PODS="$(curl -s "$BASE/api/v1/namespaces/default/pods" | python3 -c "import json,sys; print(sum(1 for i in json.load(sys.stdin).get('items',[]) if i['metadata']['name'].startswith('golden-casc')))" 2>/dev/null || echo 99)"
  if [[ "$RS" -eq 0 && "$PODS" -eq 0 ]]; then G21_CASC=1; break; fi
  sleep 0.25
done
G21_NS_CREATE="$(curl -s -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G21NS" "$BASE/api/v1/namespaces")"
G21_NS_FIN=0
printf '%s' "$G21_NS_CREATE" | grep -q '"finalizers":\["kubernetes"\]' && G21_NS_FIN=1
curl -s -o /dev/null -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G21CM" "$BASE/api/v1/namespaces/golden-ns/configmaps"
G21_NS_DEL="$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/v1/namespaces/golden-ns")"
G21_DRAIN=0
for _ in $(seq 1 80); do
  CM="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/namespaces/golden-ns/configmaps/golden-cm")"
  NS="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/namespaces/golden-ns")"
  if [[ "$CM" == "404" && "$NS" == "404" ]]; then G21_DRAIN=1; break; fi
  sleep 0.25
done
if [[ "$G21_CREATE" == "201" && "$G21_DEL" == "200" && $G21_CASC -eq 1 && $G21_NS_FIN -eq 1 \
      && "$G21_NS_DEL" == "200" && $G21_DRAIN -eq 1 ]]; then
  ok "G21  gc cascade deployment->rs->pods + namespace drain+finalize  (controllers, T3.1b)"
  PASS=$((PASS+1))
else
  echo "FAIL G21  gc/namespace (create=$G21_CREATE del=$G21_DEL casc=$G21_CASC nsfin=$G21_NS_FIN nsdel=$G21_NS_DEL drain=$G21_DRAIN)"
  FAIL=$((FAIL+1))
fi
rm -f "$TMP_G21D" "$TMP_G21NS" "$TMP_G21CM"
rm -rf "$KCTL_DIR"

# --- T3.2: kube-scheduler placement + unschedulable (G22/G23) ---
# G22: default plugins through the live server. Two nodes (one labeled
# disk=ssd); a nodeSelector pod must land on the labeled node and carry
# PodScheduled=True; an impossible selector must leave PodScheduled=False
# with reason=Unschedulable (no hot loop -- condition settles once).
TMP_G22NA="$(mktemp)"; TMP_G22NB="$(mktemp)"; TMP_G22P="$(mktemp)"; TMP_G22Q="$(mktemp)"
printf '%s' '{"apiVersion":"v1","kind":"Node","metadata":{"name":"g22-a","labels":{"disk":"ssd"}}}' > "$TMP_G22NA"
printf '%s' '{"apiVersion":"v1","kind":"Node","metadata":{"name":"g22-b"}}' > "$TMP_G22NB"
printf '%s' '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g22-fit","namespace":"default"},"spec":{"nodeSelector":{"disk":"ssd"},"containers":[{"name":"c","image":"pause"}]}}' > "$TMP_G22P"
printf '%s' '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g22-nofit","namespace":"default"},"spec":{"nodeSelector":{"disk":"nowhere"},"containers":[{"name":"c","image":"pause"}]}}' > "$TMP_G22Q"
G22_NA="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G22NA" "$BASE/api/v1/nodes")"
G22_NB="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G22NB" "$BASE/api/v1/nodes")"
G22_P="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G22P" "$BASE/api/v1/namespaces/default/pods")"
G22_Q="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G22Q" "$BASE/api/v1/namespaces/default/pods")"
G22_BOUND=0
for _ in $(seq 1 80); do
  N="$(curl -s "$BASE/api/v1/namespaces/default/pods/g22-fit" | python3 -c "import json,sys; print(json.load(sys.stdin).get('spec',{}).get('nodeName',''))" 2>/dev/null || true)"
  if [[ "$N" == "g22-a" ]]; then G22_BOUND=1; break; fi
  sleep 0.25
done
G22_SCHED_TRUE=0
curl -s "$BASE/api/v1/namespaces/default/pods/g22-fit" | python3 -c "
import json,sys
cs=json.load(sys.stdin).get('status',{}).get('conditions',[])
raise SystemExit(0 if any(c.get('type')=='PodScheduled' and c.get('status')=='True' for c in cs) else 1)" 2>/dev/null && G22_SCHED_TRUE=1
G22_UNSCHED=0
for _ in $(seq 1 80); do
  if curl -s "$BASE/api/v1/namespaces/default/pods/g22-nofit" | python3 -c "
import json,sys
cs=json.load(sys.stdin).get('status',{}).get('conditions',[])
raise SystemExit(0 if any(c.get('type')=='PodScheduled' and c.get('status')=='False' and c.get('reason')=='Unschedulable' for c in cs) else 1)" 2>/dev/null; then
    G22_UNSCHED=1; break
  fi
  sleep 0.25
done
if [[ "$G22_NA" == "201" && "$G22_NB" == "201" && "$G22_P" == "201" && "$G22_Q" == "201" \
      && "$G22_BOUND" -eq 1 && "$G22_SCHED_TRUE" -eq 1 && "$G22_UNSCHED" -eq 1 ]]; then
  ok "G22  scheduler nodeSelector placement + PodScheduled=True + Unschedulable  (scheduler, T3.2)"
  PASS=$((PASS+1))
else
  echo "FAIL G22  scheduler placement (na=$G22_NA nb=$G22_NB p=$G22_P q=$G22_Q bound=$G22_BOUND true=$G22_SCHED_TRUE unsched=$G22_UNSCHED)"
  FAIL=$((FAIL+1))
fi
rm -f "$TMP_G22NA" "$TMP_G22NB" "$TMP_G22P" "$TMP_G22Q"

# G23: the HTTP extender seam (Q3/Q23). A python3 stub extender rejects
# g23-a in its filter verb; a second server booted with
# `--kube-scheduler-arg config=<KubeSchedulerConfiguration>` must place the
# pod on the stub-approved node only.
G23=0
if ! command -v python3 >/dev/null 2>&1; then
  echo "FAIL G23  python3 required for the stub extender"
  FAIL=$((FAIL+1))
else
  STUB_PORT="$(pick_port)"
  SRV2_PORT="$(pick_port)"
  TMP_G23STUB="$(mktemp)"; TMP_G23CFG="$(mktemp)"; DD2="$(mktemp -d)"; LOG2="$(mktemp)"
  cat > "$TMP_G23STUB" <<'PYSTUB'
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(n) or b"{}")
        items = body.get("nodes", {}).get("Items", [])
        names = [i.get("metadata", {}).get("name", "") for i in items]
        if self.path == "/filter":
            resp = {"NodeNames": [x for x in names if x == "g23-b"]}
        else:
            resp = [{"host": x, "score": 100 if x == "g23-b" else 0} for x in names]
        data = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def log_message(self, *a):
        pass

HTTPServer(("127.0.0.1", int(__import__("sys").argv[1])), H).serve_forever()
PYSTUB
  python3 "$TMP_G23STUB" "$STUB_PORT" >/dev/null 2>&1 &
  STUB_PID=$!
  printf '{"apiVersion":"kubescheduler.config.k8s.io/v1","kind":"KubeSchedulerConfiguration","extenders":[{"urlPrefix":"http://127.0.0.1:%s","filterVerb":"filter","prioritizeVerb":"prioritize","weight":1,"ignorable":false}]}' "$STUB_PORT" > "$TMP_G23CFG"
  "$BIN" server --data-dir "$DD2" --bind-address 127.0.0.1 --https-listen-port "$SRV2_PORT" \
    --kube-scheduler-arg "config=$TMP_G23CFG" >"$LOG2" 2>&1 &
  SRV2=$!
  BASE2="http://127.0.0.1:${SRV2_PORT}"
  for _ in $(seq 1 60); do grep -q "discovery listening" "$LOG2" && break; sleep 0.1; done
  if grep -q "scheduler extenders loaded" "$LOG2"; then
    curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
      -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"g23-a"}}' "$BASE2/api/v1/nodes"
    curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
      -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"g23-b"}}' "$BASE2/api/v1/nodes"
    curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
      -d '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g23-p","namespace":"default"},"spec":{"containers":[{"name":"c","image":"pause"}]}}' "$BASE2/api/v1/namespaces/default/pods"
    for _ in $(seq 1 80); do
      N="$(curl -s "$BASE2/api/v1/namespaces/default/pods/g23-p" | python3 -c "import json,sys; print(json.load(sys.stdin).get('spec',{}).get('nodeName',''))" 2>/dev/null || true)"
      if [[ "$N" == "g23-b" ]]; then G23=1; break; fi
      if [[ "$N" == "g23-a" ]]; then break; fi
      sleep 0.25
    done
  fi
  if [[ "$G23" -eq 1 ]]; then
    ok "G23  extender filter steers placement via --kube-scheduler-arg config  (scheduler, T3.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G23  extender placement (check $LOG2)"
    FAIL=$((FAIL+1))
  fi
  kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; SRV2=""
  kill "$STUB_PID" 2>/dev/null || true; wait "$STUB_PID" 2>/dev/null || true; STUB_PID=""
  rm -f "$TMP_G23STUB" "$TMP_G23CFG"; rm -rf "$DD2" "$LOG2"
fi

# --- T4.1: agent-supervised containerd + crictl over CRI (G24) ---
# The agent stages the vendored runtime tree (Q24) and supervises containerd
# through the multicall seam (Q25); `crictl` must reach the CRI socket purely
# via INIT_PRO_DATA_DIR (k3s agent parity). The image-pull smoke is
# best-effort (Q25): without a reachable registry it SKIPs and does NOT
# touch FAIL — but the supervision + CRI checks themselves hard-fail WHEN a
# vendor bundle is present. CI builds with INIT_PRO_VENDOR=0 ship no
# vendor/bin tree (ci.yml lint-test), so the whole gate SKIPs then — aligned
# with the Q25 SKIP-not-fail policy and the runtime integration test: a
# missing vendor bundle is a build-configuration state, not a conformance
# break. Vendor detection mirrors runtime::stage::vendor_bin_root():
# INIT_PRO_VENDOR_BIN override, else exe-relative, else cwd-relative.
G24_VENDOR_BIN=""
if [[ -n "${INIT_PRO_VENDOR_BIN:-}" && -f "$INIT_PRO_VENDOR_BIN/containerd" ]]; then
  G24_VENDOR_BIN="$INIT_PRO_VENDOR_BIN"
else
  for c in "$(dirname "$BIN")/../../vendor/bin" "$(dirname "$BIN")/../vendor/bin" "$PWD/vendor/bin" "$ROOT/vendor/bin"; do
    if [[ -f "$c/containerd" ]]; then G24_VENDOR_BIN="$c"; break; fi
  done
fi
if [[ -z "$G24_VENDOR_BIN" ]]; then
  echo "SKIP G24 agent runtime (no vendored containerd; build with INIT_PRO_VENDOR=1)"
else
  G24=0
  G24TMP="$(mktemp -d)"; LOG24="$(mktemp)"
  "$BIN" agent --data-dir "$G24TMP/g24-dd" --token dev --server https://127.0.0.1:6443 >"$LOG24" 2>&1 &
  AGENT_PID=$!
  for _ in $(seq 1 200); do grep -q "containerd healthy" "$LOG24" && break; sleep 0.1; done
  G24_V="$(INIT_PRO_DATA_DIR="$G24TMP/g24-dd" "$BIN" crictl version 2>&1 || true)"
  if INIT_PRO_DATA_DIR="$G24TMP/g24-dd" "$BIN" crictl ps >/dev/null 2>&1; then G24_PS=1; else G24_PS=0; fi
  if grep -q "containerd healthy" "$LOG24" && grep -q "RuntimeName" <<<"$G24_V" \
     && grep -q "containerd" <<<"$G24_V" && [[ "$G24_PS" -eq 1 ]]; then
    ok "G24  agent supervises containerd; crictl version/ps over CRI  (runtime, T4.1)"
    PASS=$((PASS+1)); G24=1
  else
    echo "FAIL G24  agent runtime (check $LOG24; version: $G24_V ps=$G24_PS)"
    FAIL=$((FAIL+1))
  fi
  if [[ "$G24" -eq 1 ]]; then
    if timeout 60 env INIT_PRO_DATA_DIR="$G24TMP/g24-dd" "$BIN" crictl pull registry.k8s.io/pause:3.10 >/dev/null 2>&1; then
      ok "G24  crictl pull pause sandbox image over CRI  (runtime, T4.1)"
      PASS=$((PASS+1))
    else
      echo "SKIP g24 sandbox smoke (registry unreachable)"
    fi
  fi
  kill -TERM "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; AGENT_PID=""
  rm -rf "$G24TMP" "$LOG24"
fi

# --- T4.2: kubelet runs a real pod end-to-end (G25) ---
# Same vendor gate as G24 (SKIP without a vendored containerd) plus a `cc`
# gate: the workload image is the Q27 airgap pause — assembled locally from
# a static binary (scripts/build-pause-image.sh), imported through the
# staged `ctr`, never pulled from a registry. Flow: Deployment -> RS -> pod
# -> scheduler binds g25-node -> kubelet drives CRI -> Running+Ready;
# kill the container -> kubelet restarts it; delete the Deployment -> GC +
# kubelet teardown empty every sandbox.
if [[ -z "$G24_VENDOR_BIN" ]]; then
  echo "SKIP G25 kubelet end-to-end (no vendored containerd)"
elif ! command -v cc >/dev/null 2>&1; then
  echo "SKIP G25 kubelet end-to-end (no cc for the airgap pause image, Q27)"
else
  G25TMP="$(mktemp -d)"; G25SRV="$(mktemp)"; G25AG="$(mktemp)"
  G25_PORT="$(pick_port)"
  "$BIN" server --data-dir "$G25TMP/srv" --bind-address 127.0.0.1 --https-listen-port "$G25_PORT" >"$G25SRV" 2>&1 &
  SRV2=$!
  for _ in $(seq 1 60); do grep -q "discovery listening" "$G25SRV" && break; sleep 0.1; done
  BASE25="http://127.0.0.1:${G25_PORT}"
  PAUSE_REF="init-pro.local/pause:0.1"
  G25_IMG="$G25TMP/pause.tar"
  "$SCRIPT_DIR/build-pause-image.sh" "$G25_IMG" "$PAUSE_REF" >/dev/null 2>&1 || true
  INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" "$BIN" agent --data-dir "$G25TMP/ag" --token dev \
      --server "$BASE25" --node-name g25-node >"$G25AG" 2>&1 &
  AGENT_PID=$!
  for _ in $(seq 1 300); do grep -q "containerd healthy" "$G25AG" && break; sleep 0.1; done
  INIT_PRO_DATA_DIR="$G25TMP/ag" "$BIN" ctr -n k8s.io images import "$G25_IMG" >/dev/null 2>&1 || true
  curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
    -d '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"g25-dep","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"g25"}},"template":{"metadata":{"labels":{"app":"g25"}},"spec":{"containers":[{"name":"pause","image":"'"$PAUSE_REF"'"}]}}}}' \
    "$BASE25/apis/apps/v1/namespaces/default/deployments"
  G25A=0
  for _ in $(seq 1 240); do
    S="$(curl -s "$BASE25/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for p in items:
  st=p.get("status",{})
  conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
  if st.get("phase")=="Running" and conds.get("Ready")=="True": print("READY"); break' 2>/dev/null || true)"
    [[ "$S" == "READY" ]] && { G25A=1; break; }
    sleep 0.5
  done
  if [[ "$G25A" -eq 1 ]]; then
    ok "G25  deployment pod Running+Ready on the agent node  (kubelet, T4.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G25a pod never Running+Ready (server: $G25SRV agent: $G25AG)"
    FAIL=$((FAIL+1))
  fi
  G25B=0
  if [[ "$G25A" -eq 1 ]]; then
    CID="$(INIT_PRO_DATA_DIR="$G25TMP/ag" "$BIN" crictl ps -q 2>/dev/null | head -1)"
    if [[ -n "$CID" ]]; then
      INIT_PRO_DATA_DIR="$G25TMP/ag" "$BIN" crictl stop --timeout 5 "$CID" >/dev/null 2>&1 || true
      for _ in $(seq 1 120); do
        NID="$(INIT_PRO_DATA_DIR="$G25TMP/ag" "$BIN" crictl ps -q 2>/dev/null | head -1)"
        if [[ -n "$NID" && "$NID" != "$CID" ]]; then G25B=1; break; fi
        sleep 0.5
      done
    fi
  fi
  if [[ "$G25B" -eq 1 ]]; then
    ok "G25  killed container restarted by the kubelet  (kubelet, T4.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G25b container restart (agent log: $G25AG)"
    FAIL=$((FAIL+1))
  fi
  G25C=0
  curl -s -o /dev/null -X DELETE "$BASE25/apis/apps/v1/namespaces/default/deployments/g25-dep"
  for _ in $(seq 1 120); do
    CNT="$(INIT_PRO_DATA_DIR="$G25TMP/ag" "$BIN" crictl pods -o json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null || echo 99)"
    [[ "$CNT" == "0" ]] && { G25C=1; break; }
    sleep 0.5
  done
  if [[ "$G25C" -eq 1 ]]; then
    ok "G25  deployment delete tears pods down to zero sandboxes  (kubelet, T4.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G25c teardown left sandboxes (count=$CNT; agent log: $G25AG)"
    FAIL=$((FAIL+1))
  fi
  kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; SRV2=""
  kill -TERM "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; AGENT_PID=""
  rm -rf "$G25TMP" "$G25SRV" "$G25AG"
fi

echo
echo "golden: $PASS passed, $FAIL failed"
if [[ "$FAIL" -ne 0 ]]; then
  echo "golden conformance FAILED" >&2
  exit 1
fi
echo "ALL golden conformance checks passed"
