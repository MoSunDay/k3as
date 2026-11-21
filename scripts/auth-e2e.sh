#!/usr/bin/env bash
# T1.3a + T1.3b (Q47, Q59): real-machine auth e2e evidence gate. Boots ONE
# `init-pro server` armed with `--token-file` + `--tls-san localhost` (plus
# a short-lived fail-closed probe server) and asserts, over the REAL
# apiserver HTTPS surface:
#
#   A1  static-token authn: anonymous resource GET -> 401 with a k8s Status
#       body (`"reason":"Unauthorized"`); an unknown bearer token -> 401.
#   A2  RBAC authz (admin = system:masters): POST configmap 201, GET 200,
#       DELETE 200 (re-GET 404); groupless token (nobody) GET configmap ->
#       403 Status carrying `forbidden` + `cannot get resource`.
#   A3  kubelet chain (P3, system:nodes): POST /api/v1/nodes 201 and
#       GET /api/v1/nodes 200.
#   A4  non-resource authenticated-allow: nobody GET /apis and /api/v1 ->
#       200 both; anonymous /apis -> 401.
#   A5  conditional discovery (auth armed): /apis lists
#       rbac.authorization.k8s.io, /api/v1 lists serviceaccounts, and the
#       bootstrap ClusterRoles cluster-admin + system:node GET 200.
#   A6  TLS (Q47): CA on disk non-empty; https + CA + token /api -> 200;
#       https anonymous -> 401; plain http against the TLS port fails
#       (curl rc != 0); kubectl over a kubeconfig (https + CA + token)
#       api-versions lists apps/v1, a bad token -> `HTTP 401`, a kubeconfig
#       missing certificate-authority fails with the explicit Q47 error.
#   A7  fail-closed: `--token-file` pointing at a missing file -> the
#       server exits by itself within 2s, log carries
#       `refusing to serve unauthenticated`.
#
# SKIP doctrine: missing $BIN / curl / python3 -> one SKIP line, exit 0
# (python3 mints the per-run random tokens). This gate needs NO root, NO
# vendor bundle, NO cluster agents — it is apiserver-surface only.
#
# Usage: scripts/auth-e2e.sh
#   INIT_PRO_BIN=...  reuse a specific binary (default target/debug/init-pro)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

for need in "$BIN" curl python3; do
  if ! command -v "$need" >/dev/null 2>&1; then
    echo "SKIP auth-e2e: missing prerequisite '$need' (build first / install it)"
    exit 0
  fi
done

pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
    || echo $((20000 + RANDOM % 10000))
}

rand_token() { python3 -c 'import secrets; print(secrets.token_hex(12))'; }

stop_pid() {
  local p="$1" i
  [[ -n "$p" ]] || return 0
  kill -TERM "$p" 2>/dev/null || true
  for i in $(seq 1 50); do
    kill -0 "$p" 2>/dev/null || return 0
    sleep 0.1
  done
  kill -KILL "$p" 2>/dev/null || true
  wait "$p" 2>/dev/null || true
}

PASS=0; FAIL=0
ok()  { echo "OK    $*"; PASS=$((PASS+1)); }
bad() { echo "FAIL  $*"; FAIL=$((FAIL+1)); }

API_PORT="$(pick_port)"   # main auth+TLS apiserver
FC_PORT="$(pick_port)"    # fail-closed probe (never binds)
BASE="https://127.0.0.1:$API_PORT"
TMP="$(mktemp -d /tmp/init-pro-auth-e2e.XXXXXX)"
DATA="$TMP/server-data"
CA="$DATA/server/tls/server-ca.crt"
CSV="$TMP/tokens.csv"
SRV_LOG="$TMP/server.log"; FC_LOG="$TMP/failclosed.log"
SRV_PID=""; FC_PID=""

cleanup() {
  trap - EXIT INT TERM
  echo
  echo "auth-e2e: tearing down ..."
  stop_pid "$FC_PID"
  stop_pid "$SRV_PID"
  if [[ "${KEEP_TMP:-0}" == "1" ]]; then
    echo "auth-e2e: logs kept at $TMP (KEEP_TMP=1)"
  else
    rm -rf "$TMP" 2>/dev/null || true
  fi
  echo "auth-e2e: bye."
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# --- static token file (kubeadm CSV: token,user,uid,"groups") ----------------
ADMIN_TOKEN="$(rand_token)"
NOBODY_TOKEN="$(rand_token)"
NODE_TOKEN="$(rand_token)"
cat > "$CSV" <<EOF
# auth-e2e static tokens (kubeadm format, Q47)
$ADMIN_TOKEN,admin,e2e-admin-uid,"system:masters"
$NOBODY_TOKEN,nobody,e2e-nobody-uid,""
$NODE_TOKEN,kubelet-e2e,e2e-node-uid,"system:nodes"
EOF

# --- boot the main auth+TLS server -------------------------------------------
# Lean plane: the probes hit the apiserver HTTP surface only, so every
# optional component is disabled for a fast, quiet boot.
"$BIN" server \
  --data-dir "$DATA" \
  --bind-address 127.0.0.1 \
  --https-listen-port "$API_PORT" \
  --token-file "$CSV" \
  --tls-san localhost \
  --disable-controller-manager \
  --disable-scheduler \
  --disable-kube-proxy \
  --disable-anyhome \
  --disable-registry >"$SRV_LOG" 2>&1 &
SRV_PID=$!

SRV_UP=0
for _ in $(seq 1 120); do
  kill -0 "$SRV_PID" 2>/dev/null || break
  grep -q "discovery listening" "$SRV_LOG" 2>/dev/null && { SRV_UP=1; break; }
  sleep 0.25
done
if [[ "$SRV_UP" != "1" ]]; then
  echo "--- server log ($SRV_LOG) ---"; cat "$SRV_LOG" || true
  echo "auth-e2e: $PASS passed, $FAIL failed"
  exit 1
fi
echo "auth-e2e: server up on $BASE (token-file + tls-san armed)"

# https_code METHOD PATH TOKEN OUTFILE -> prints the HTTP status code.
# The CA is the ONLY trust root (--cacert replaces the system bundle).
https_code() {
  local method="$1" path="$2" token="$3" out="$4"
  if [[ -n "$token" ]]; then
    curl -s -o "$out" -w '%{http_code}' --max-time 10 --cacert "$CA" \
      -X "$method" -H "Authorization: Bearer $token" "$BASE$path"
  else
    curl -s -o "$out" -w '%{http_code}' --max-time 10 --cacert "$CA" \
      -X "$method" "$BASE$path"
  fi
}

PODS="/api/v1/namespaces/default/pods"
CM="/api/v1/namespaces/default/configmaps/e2e-auth-cm"
CM_BODY='{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"e2e-auth-cm","namespace":"default"},"data":{"k":"v"}}'
NODE_BODY='{"apiVersion":"v1","kind":"Node","metadata":{"name":"e2e-auth-node"},"spec":{}}'

post_json() { # PATH TOKEN OUTFILE BODY
  curl -s -o "$3" -w '%{http_code}' --max-time 10 --cacert "$CA" -X POST \
    -H "Authorization: Bearer $2" -H 'Content-Type: application/json' \
    --data-binary "$4" "$BASE$1"
}

# --- A1: static-token authentication -----------------------------------------
A1_ANON="$(https_code GET "$PODS" "" "$TMP/a1-anon.json" || echo 000)"
A1_BAD="$(https_code GET "$PODS" "definitely-not-a-token" "$TMP/a1-bad.json" || echo 000)"
if [[ "$A1_ANON" == "401" && "$A1_BAD" == "401" ]] \
   && grep -qF '"reason":"Unauthorized"' "$TMP/a1-anon.json" \
   && grep -qF '"reason":"Unauthorized"' "$TMP/a1-bad.json"; then
  ok "A1  anonymous + unknown token -> 401 with Status reason=Unauthorized"
else
  bad "A1  authn probe (anon=$A1_ANON bad=$A1_BAD; anon body: $(head -c 120 "$TMP/a1-anon.json" 2>/dev/null))"
fi

# --- A2: RBAC authorization ---------------------------------------------------
A2_POST="$(post_json "/api/v1/namespaces/default/configmaps" "$ADMIN_TOKEN" "$TMP/a2-post.json" "$CM_BODY" || echo 000)"
A2_GET="$(https_code GET "$CM" "$ADMIN_TOKEN" "$TMP/a2-get.json" || echo 000)"
A2_DEL="$(https_code DELETE "$CM" "$ADMIN_TOKEN" "$TMP/a2-del.json" || echo 000)"
A2_REGET="$(https_code GET "$CM" "$ADMIN_TOKEN" "$TMP/a2-reget.json" || echo 000)"
A2_NOBODY="$(https_code GET "$CM" "$NOBODY_TOKEN" "$TMP/a2-nobody.json" || echo 000)"
if [[ "$A2_POST" == "201" && "$A2_GET" == "200" && "$A2_DEL" == "200" && "$A2_REGET" == "404" ]] \
   && [[ "$A2_NOBODY" == "403" ]] \
   && grep -qiF 'forbidden' "$TMP/a2-nobody.json" \
   && grep -qF 'cannot get resource' "$TMP/a2-nobody.json"; then
  ok "A2  admin CRUD configmap 201/200/200 (+re-GET 404); nobody GET -> 403 forbidden"
else
  bad "A2  rbac probe (post=$A2_POST get=$A2_GET del=$A2_DEL reget=$A2_REGET nobody=$A2_NOBODY; body: $(head -c 160 "$TMP/a2-nobody.json" 2>/dev/null))"
fi

# --- A3: kubelet chain (system:nodes, P3) ------------------------------------
A3_POST="$(post_json "/api/v1/nodes" "$NODE_TOKEN" "$TMP/a3-post.json" "$NODE_BODY" || echo 000)"
A3_LIST="$(https_code GET "/api/v1/nodes" "$NODE_TOKEN" "$TMP/a3-list.json" || echo 000)"
if [[ "$A3_POST" == "201" && "$A3_LIST" == "200" ]]; then
  ok "A3  system:nodes token created a Node (201) and listed nodes (200)"
else
  bad "A3  kubelet chain (post=$A3_POST list=$A3_LIST; body: $(head -c 160 "$TMP/a3-post.json" 2>/dev/null))"
fi

# --- A4: non-resource paths are authenticated-allow ---------------------------
A4_APIS="$(https_code GET "/apis" "$NOBODY_TOKEN" "$TMP/a4-apis.json" || echo 000)"
A4_APIV1="$(https_code GET "/api/v1" "$NOBODY_TOKEN" "$TMP/a4-apiv1.json" || echo 000)"
A4_ANON="$(https_code GET "/apis" "" "$TMP/a4-anon.json" || echo 000)"
if [[ "$A4_APIS" == "200" && "$A4_APIV1" == "200" && "$A4_ANON" == "401" ]]; then
  ok "A4  nobody GET /apis + /api/v1 -> 200 (authenticated-allow); anonymous /apis -> 401"
else
  bad "A4  non-resource probe (apis=$A4_APIS apiv1=$A4_APIV1 anon=$A4_ANON)"
fi

# --- A5: discovery registers the auth-only groups/resources -------------------
A5_APIS="$(https_code GET "/apis" "$ADMIN_TOKEN" "$TMP/a5-apis.json" || echo 000)"
A5_APIV1="$(https_code GET "/api/v1" "$ADMIN_TOKEN" "$TMP/a5-apiv1.json" || echo 000)"
A5_CA="$(https_code GET "/apis/rbac.authorization.k8s.io/v1/clusterroles/cluster-admin" "$ADMIN_TOKEN" "$TMP/a5-ca.json" || echo 000)"
A5_SN="$(https_code GET "/apis/rbac.authorization.k8s.io/v1/clusterroles/system:node" "$ADMIN_TOKEN" "$TMP/a5-sn.json" || echo 000)"
if [[ "$A5_APIS" == "200" && "$A5_APIV1" == "200" && "$A5_CA" == "200" && "$A5_SN" == "200" ]] \
   && grep -qF 'rbac.authorization.k8s.io' "$TMP/a5-apis.json" \
   && grep -qF 'serviceaccounts' "$TMP/a5-apiv1.json"; then
  ok "A5  auth-armed discovery: rbac group + serviceaccounts listed; cluster-admin + system:node bootstrapped (200)"
else
  bad "A5  discovery probe (apis=$A5_APIS apiv1=$A5_APIV1 cluster-admin=$A5_CA system:node=$A5_SN)"
fi

# --- A6: TLS serving plane + kubectl kubeconfig -------------------------------
ln -sf "$BIN" "$TMP/kubectl"
KC_GOOD="$TMP/kubeconfig.yaml"
KC_BADTOK="$TMP/kubeconfig-badtoken.yaml"
KC_NOCA="$TMP/kubeconfig-noca.yaml"
sed -e "s|@SERVER@|$BASE|" -e "s|@CA@|$CA|" -e "s|@TOKEN@|$ADMIN_TOKEN|" > "$KC_GOOD" <<'KCEOF'
apiVersion: v1
kind: Config
current-context: auth-e2e
contexts:
- name: auth-e2e
  context:
    cluster: auth-e2e
    user: admin
clusters:
- name: auth-e2e
  cluster:
    server: @SERVER@
    certificate-authority: @CA@
users:
- name: admin
  user:
    token: @TOKEN@
KCEOF
sed "s|$ADMIN_TOKEN|definitely-not-a-token|" "$KC_GOOD" > "$KC_BADTOK"
grep -v 'certificate-authority' "$KC_GOOD" > "$KC_NOCA"

KC_OUT="$(KUBECONFIG="$KC_GOOD" timeout 30 "$TMP/kubectl" api-versions 2>"$TMP/kc-good.err")" && KC_RC=0 || KC_RC=$?
KC_BAD_OUT="$(KUBECONFIG="$KC_BADTOK" timeout 30 "$TMP/kubectl" api-versions 2>"$TMP/kc-bad.err")" && KC_BAD_RC=0 || KC_BAD_RC=$?
KC_NOCA_OUT="$(KUBECONFIG="$KC_NOCA" timeout 30 "$TMP/kubectl" api-versions 2>"$TMP/kc-noca.err")" && KC_NOCA_RC=0 || KC_NOCA_RC=$?

A6_HTTP_CODE="$(https_code GET "/api" "$ADMIN_TOKEN" "$TMP/a6-api.json" || echo 000)"
A6_ANON="$(https_code GET "/api" "" "$TMP/a6-anon.json" || echo 000)"
if curl -s -o /dev/null --max-time 5 "http://127.0.0.1:$API_PORT/api" 2>/dev/null; then
  A6_PLAIN=0
else
  A6_PLAIN=$?
fi
A6_CA_OK=0; [[ -s "$CA" ]] && A6_CA_OK=1
if [[ "$A6_CA_OK" == "1" && "$A6_HTTP_CODE" == "200" && "$A6_ANON" == "401" && "$A6_PLAIN" != "0" ]] \
   && [[ "$KC_RC" == "0" && "$(printf '%s\n' "$KC_OUT" | grep -cF 'apps/v1')" -ge 1 ]] \
   && [[ "$KC_BAD_RC" != "0" && "$(grep -cF 'HTTP 401' "$TMP/kc-bad.err")" -ge 1 ]] \
   && [[ "$KC_NOCA_RC" != "0" && "$(grep -cF 'carries no certificate-authority' "$TMP/kc-noca.err")" -ge 1 ]]; then
  ok "A6  TLS plane: CA on disk, https+CA+token /api 200, anonymous 401, plain-http rejected (rc=$A6_PLAIN); kubectl kubeconfig https ok / bad token 401 / missing CA explicit error"
else
  bad "A6  tls/kubectl probe (ca=$A6_CA_OK api=$A6_HTTP_CODE anon=$A6_ANON plain_rc=$A6_PLAIN kc=$KC_RC badtok=$KC_BAD_RC:$(head -c 80 "$TMP/kc-bad.err" 2>/dev/null) noca=$KC_NOCA_RC:$(head -c 120 "$TMP/kc-noca.err" 2>/dev/null))"
fi

# --- A7: fail-closed on an unreadable token file ------------------------------
"$BIN" server \
  --data-dir "$TMP/failclosed-data" \
  --bind-address 127.0.0.1 \
  --https-listen-port "$FC_PORT" \
  --token-file "$TMP/definitely-no-such-tokens.csv" \
  --disable-controller-manager \
  --disable-scheduler \
  --disable-kube-proxy \
  --disable-anyhome \
  --disable-registry >"$FC_LOG" 2>&1 &
FC_PID=$!
A7_DIED=0; A7_ELAPSED=999
FC_T0="$(date +%s%N)"
for _ in $(seq 1 40); do
  kill -0 "$FC_PID" 2>/dev/null || { A7_DIED=1; break; }
  sleep 0.1
done
FC_T1="$(date +%s%N)"
A7_ELAPSED=$(( (FC_T1 - FC_T0) / 1000000 ))
wait "$FC_PID" 2>/dev/null && A7_RC=0 || A7_RC=$?
FC_PID=""
if [[ "$A7_DIED" == "1" && "$A7_ELAPSED" -le 2000 && "$A7_RC" != "0" ]] \
   && grep -qF 'refusing to serve unauthenticated' "$FC_LOG"; then
  ok "A7  missing token-file -> server self-exited in ${A7_ELAPSED}ms (rc=$A7_RC) with 'refusing to serve unauthenticated'"
else
  stop_pid "$FC_PID"
  bad "A7  fail-closed probe (died=$A7_DIED elapsed=${A7_ELAPSED}ms rc=$A7_RC; log: $(tr '\n' '|' < "$FC_LOG" 2>/dev/null | head -c 200))"
fi

# --- summary ------------------------------------------------------------------
echo
echo "auth-e2e: $PASS passed, $FAIL failed"
if [[ "$FAIL" -eq 0 ]]; then
  echo "ALL auth e2e checks passed (A1-A7)"
  exit 0
fi
echo "auth-e2e FAILED" >&2
exit 1
