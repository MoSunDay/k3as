# Storage suite (G06-G15): empty-cluster collection list + CRUD round-trip
# (T1.2b), Server-Side Apply (T1.2c), watch historical replay (T2.2).
# Sourced by golden-conformance.sh; runs in the main script's shell.
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
