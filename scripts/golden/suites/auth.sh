# T1.3 (Q47/Q59) auth suite: the server-side wire surface of static-token
# authn (authn.rs middleware), RBAC authz (authz.rs evaluator + the
# bootstrapped cluster-admin/system:node policy), non-resource
# authenticated-allow, and the auth-armed discovery posture (Q47: arming
# adds rbac.authorization.k8s.io/v1 + core serviceaccounts; the default
# no-auth posture stays byte-identical, golden G01-G04 red line).
# IMPLEMENTED semantics; every check asserts INTENDED-vs-ACTUAL via status
# code + a stable message substring. Sourced by golden-conformance.sh; runs
# in the main script's shell (helpers: ok, counters PASS/FAIL, $BASE = the
# UN-armed main server, $BIN, pick_port; SRV2 = this suite's own
# auth-armed second server — killed + cleared here, the main cleanup only
# backstops it). Owned by plan item: T1.3 (Q47/Q59).
# Full-stack evidence (token plumbing A1-A7: kubelet/agent auth against a
# token-file server, TLS opt-in, live kubectl) lives in scripts/auth-e2e.sh;
# this suite pins only the apiserver wire level.
# Check-ID range reserved: G107-G113. Hard cap 400 lines.
#
#   G107  anonymous + unknown bearer -> 401 both, body is a Kubernetes
#        Status with "reason":"Unauthorized".
#   G108  admin (system:masters) CRUD: POST / GET / DELETE configmap ->
#        201 / 200 / 200.
#   G109  nobody (empty group row) GET configmap -> 403, message pins
#        "forbidden" + "cannot get resource" (Kubernetes-style Status).
#   G110  kubelet chain (system:nodes -> bootstrapped system:node):
#        POST /api/v1/nodes 201 + GET node 200.
#   G111  non-resource authenticated-allow: nobody GET /apis and /api/v1
#        -> 200 both; anonymous /apis -> 401 (authn still gates).
#   G112  auth-armed discovery: admin /apis serves rbac.authorization.k8s.io,
#        /api/v1 serves serviceaccounts, GET cluster-admin ClusterRole 200;
#        CROSS-pin: the un-armed main server ($BASE) serves NEITHER —
#        the default posture is byte-unchanged by T1.3.
#   G113  stability + teardown: the admin token still yields 200 right
#        before the suite kills its own SRV2 (auth plane stable all run).

AUTH_TMP="$(mktemp -d /tmp/golden-auth.XXXXXX)"
AUTH_CSV="$AUTH_TMP/tokens.csv"
# kubeadm CSV (P2): token,user,uid,"groups" — third row = quoted EMPTY
# group list (the nobody identity must carry zero groups end to end).
{
  printf '%s\n' 'admin-token,admin,1111,"system:masters"'
  printf '%s\n' 'kubelet-token,kubelet-n1,2222,"system:nodes"'
  printf '%s\n' 'nobody-token,nobody,3333,""'
} >"$AUTH_CSV"

AUTH_PORT="$(pick_port)"
AUTH_BASE="http://127.0.0.1:${AUTH_PORT}"
AUTH_LOG="$AUTH_TMP/srv.log"
"$BIN" server --data-dir "$AUTH_TMP/dd" --bind-address 127.0.0.1 \
    --https-listen-port "$AUTH_PORT" --token-file "$AUTH_CSV" \
    >"$AUTH_LOG" 2>&1 &
SRV2=$!
for _ in $(seq 1 100); do grep -q "discovery listening" "$AUTH_LOG" && break; sleep 0.1; done

# auth_req NAME TOKEN METHOD PATH [JSON] -> status code; the response body
# lands in $AUTH_TMP/NAME.json (TOKEN "" = anonymous, no header sent).
auth_req() {
  local name="$1" tok="$2" method="$3" path="$4" body="${5:-}"
  local args=(-s --max-time 5 -X "$method" -o "$AUTH_TMP/$name.json" -w '%{http_code}')
  [[ -n "$tok" ]] && args+=(-H "Authorization: Bearer $tok")
  [[ -n "$body" ]] && args+=(-H 'Content-Type: application/json' --data-binary "$body")
  curl "${args[@]}" "$AUTH_BASE$path" 2>/dev/null || true
}

# --- G107: anonymous + unknown token -> 401 Status -------------------------
AUTH_G107A="$(auth_req g107a "" GET /apis)"
AUTH_G107B="$(auth_req g107b wrong-token GET /apis)"
AUTH_G107_BODY="$(cat "$AUTH_TMP/g107b.json" 2>/dev/null || true)"
if [[ "$AUTH_G107A" == "401" && "$AUTH_G107B" == "401" ]] \
  && printf '%s' "$AUTH_G107_BODY" | grep -qF '"kind":"Status"' \
  && printf '%s' "$AUTH_G107_BODY" | grep -qF '"reason":"Unauthorized"'; then
  ok "G107  anonymous and unknown bearer -> 401 both, body is a Status (Unauthorized)"
  PASS=$((PASS+1))
else
  echo "FAIL G107  authn rejects (anon=$AUTH_G107A bad=$AUTH_G107B body=$AUTH_G107_BODY; expected 401/401 + Status Unauthorized)"
  FAIL=$((FAIL+1))
fi

# --- G108: admin (system:masters) CRUD round-trip ---------------------------
AUTH_G108='{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"g108-cm","namespace":"default"},"data":{"k":"v"}}'
AUTH_G108_POST="$(auth_req g108a admin-token POST /api/v1/namespaces/default/configmaps "$AUTH_G108")"
AUTH_G108_GET="$(auth_req g108b admin-token GET /api/v1/namespaces/default/configmaps/g108-cm)"
AUTH_G108_DEL="$(auth_req g108c admin-token DELETE /api/v1/namespaces/default/configmaps/g108-cm)"
if [[ "$AUTH_G108_POST" == "201" && "$AUTH_G108_GET" == "200" && "$AUTH_G108_DEL" == "200" ]]; then
  ok "G108  admin CRUD configmap -> 201 / 200 / 200 (system:masters wildcard)"
  PASS=$((PASS+1))
else
  echo "FAIL G108  admin CRUD (post=$AUTH_G108_POST get=$AUTH_G108_GET del=$AUTH_G108_DEL; expected 201/200/200)"
  FAIL=$((FAIL+1))
fi

# --- G109: nobody (no groups) -> 403 forbidden ------------------------------
AUTH_G109="$(auth_req g109 nobody-token GET /api/v1/namespaces/default/configmaps/g108-cm)"
AUTH_G109_BODY="$(cat "$AUTH_TMP/g109.json" 2>/dev/null || true)"
if [[ "$AUTH_G109" == "403" ]] \
  && printf '%s' "$AUTH_G109_BODY" | grep -qF 'forbidden' \
  && printf '%s' "$AUTH_G109_BODY" | grep -qF 'cannot get resource'; then
  ok "G109  nobody GET configmap -> 403, message pins forbidden + cannot get resource"
  PASS=$((PASS+1))
else
  echo "FAIL G109  nobody probe (code=$AUTH_G109 body=$AUTH_G109_BODY; expected 403 + forbidden message)"
  FAIL=$((FAIL+1))
fi

# --- G110: kubelet chain creates + reads its Node ---------------------------
AUTH_G110='{"apiVersion":"v1","kind":"Node","metadata":{"name":"g110-node"},"spec":{}}'
AUTH_G110_POST="$(auth_req g110a kubelet-token POST /api/v1/nodes "$AUTH_G110")"
AUTH_G110_GET="$(auth_req g110b kubelet-token GET /api/v1/nodes/g110-node)"
if [[ "$AUTH_G110_POST" == "201" && "$AUTH_G110_GET" == "200" ]]; then
  ok "G110  kubelet token POST /api/v1/nodes -> 201 + GET node -> 200 (system:nodes chain)"
  PASS=$((PASS+1))
else
  echo "FAIL G110  kubelet chain (post=$AUTH_G110_POST get=$AUTH_G110_GET; expected 201/200)"
  FAIL=$((FAIL+1))
fi

# --- G111: non-resource paths allow ANY authenticated user ------------------
AUTH_G111A="$(auth_req g111a nobody-token GET /apis)"
AUTH_G111B="$(auth_req g111b nobody-token GET /api/v1)"
AUTH_G111C="$(auth_req g111c "" GET /apis)"
if [[ "$AUTH_G111A" == "200" && "$AUTH_G111B" == "200" && "$AUTH_G111C" == "401" ]]; then
  ok "G111  nobody GET /apis and /api/v1 -> 200 both; anonymous /apis -> 401"
  PASS=$((PASS+1))
else
  echo "FAIL G111  non-resource probes (apis=$AUTH_G111A apiv1=$AUTH_G111B anon=$AUTH_G111C; expected 200/200/401)"
  FAIL=$((FAIL+1))
fi

# --- G112: auth-armed discovery + default-posture cross-pin -----------------
AUTH_G112_APIS="$(auth_req g112a admin-token GET /apis)"
AUTH_G112_APIS_BODY="$(cat "$AUTH_TMP/g112a.json" 2>/dev/null || true)"
AUTH_G112_CORE="$(auth_req g112b admin-token GET /api/v1)"
AUTH_G112_CORE_BODY="$(cat "$AUTH_TMP/g112b.json" 2>/dev/null || true)"
AUTH_G112_CR="$(auth_req g112c admin-token GET /apis/rbac.authorization.k8s.io/v1/clusterroles/cluster-admin)"
# Cross-evidence on the UN-armed main server (Q47 default-off red line).
AUTH_G112_BASE_APIS="$(curl -s --max-time 5 "$BASE/apis" 2>/dev/null || true)"
AUTH_G112_BASE_CORE="$(curl -s --max-time 5 "$BASE/api/v1" 2>/dev/null || true)"
if [[ "$AUTH_G112_APIS" == "200" && "$AUTH_G112_CR" == "200" ]] \
  && printf '%s' "$AUTH_G112_APIS_BODY" | grep -qF 'rbac.authorization.k8s.io' \
  && [[ "$AUTH_G112_CORE" == "200" ]] \
  && printf '%s' "$AUTH_G112_CORE_BODY" | grep -qF 'serviceaccounts' \
  && ! printf '%s' "$AUTH_G112_BASE_APIS" | grep -qF 'rbac.authorization.k8s.io' \
  && ! printf '%s' "$AUTH_G112_BASE_CORE" | grep -qF 'serviceaccounts'; then
  ok "G112  armed discovery serves rbac + serviceaccounts + cluster-admin 200; main server serves neither"
  PASS=$((PASS+1))
else
  echo "FAIL G112  discovery arming (apis=$AUTH_G112_APIS core=$AUTH_G112_CORE cr=$AUTH_G112_CR; armed-body=[$AUTH_G112_APIS_BODY/$AUTH_G112_CORE_BODY]; base-apis-has-rbac=$(printf '%s' "$AUTH_G112_BASE_APIS" | grep -cF 'rbac.authorization.k8s.io') base-core-has-sa=$(printf '%s' "$AUTH_G112_BASE_CORE" | grep -cF 'serviceaccounts'); expected armed=both + base=0/0)"
  FAIL=$((FAIL+1))
fi

# --- G113: auth plane stable to the end, then this suite reaps SRV2 ---------
AUTH_G113="$(auth_req g113 admin-token GET /api/v1/namespaces/default/configmaps)"
if [[ "$AUTH_G113" == "200" ]]; then
  ok "G113  admin token still 200 at teardown (auth plane stable the whole run)"
  PASS=$((PASS+1))
else
  echo "FAIL G113  late admin probe (code=$AUTH_G113; expected 200)"
  FAIL=$((FAIL+1))
fi
kill "$SRV2" 2>/dev/null || true
wait "$SRV2" 2>/dev/null || true
SRV2=""
rm -rf "$AUTH_TMP" 2>/dev/null || true
