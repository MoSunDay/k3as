# Negative/edge-surface suite (B6): eviction / deletecollection /
# labelSelector / https-reject / sqlite-Unsupported / duplicate-binding.
# Every check asserts INTENDED-vs-ACTUAL consciously and pins the deviation
# class in a trailing comment: LOCKED (Q-code) | IMPLEMENTED semantics |
# FINDING (a) (probable product bug; ledgered, never fixed test-side).
# Sourced by golden-conformance.sh; runs in the main script's shell
# (helpers: check_body/check_status/check_method/ok, counters PASS/FAIL,
# $BASE, $BIN, pick_port; `set -euo pipefail` is already on).
# Owned by plan item: B6. Check-ID range reserved: G50-G55. Hard cap 400 lines.

NG_TMP="$(mktemp -d)"

# --- G50: the pods/eviction subresource is not served ----------------------
# ACTUAL: POST .../pods/<name>/eviction -> 404 (empty body): crates/apiserver/
# src/app.rs wires pods/{status,log,exec,binding} only -- no /eviction route,
# so axum's fallback 404s. Upstream k8s serves policy/v1 Eviction here.
# FINDING (a): missing subresource (upstream-parity scope gap; note the
# discovery doc advertises NO subresources at all, so this is consistent
# with today's discovery contract -- still a Q2 conformance gap to ledger).
NG_POD='{"apiVersion":"v1","kind":"Pod","metadata":{"name":"rt-ng-evict","namespace":"default"},"spec":{"schedulerName":"golden-none","containers":[{"name":"c","image":"i"}]}}'
NG_POD_CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$NG_POD" "$BASE/api/v1/namespaces/default/pods" 2>/dev/null || true)"
NG_EV_CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary '{"apiVersion":"policy/v1","kind":"Eviction","metadata":{"name":"rt-ng-evict","namespace":"default"}}' "$BASE/api/v1/namespaces/default/pods/rt-ng-evict/eviction" 2>/dev/null || true)"
if [[ "$NG_POD_CODE" == "201" && "$NG_EV_CODE" == "404" ]]; then
  ok "G50  POST pods/rt-ng-evict/eviction -> 404  (eviction subresource not served; FINDING (a) upstream-parity gap, pinned)"
  PASS=$((PASS+1))
else
  echo "FAIL G50  eviction subresource probe (pod create=$NG_POD_CODE, eviction=$NG_EV_CODE, expected pod=201 eviction=404)"
  FAIL=$((FAIL+1))
fi
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE/api/v1/namespaces/default/pods/rt-ng-evict" 2>/dev/null || true

# --- G51: deletecollection is not served -----------------------------------
# ACTUAL: DELETE on a collection path -> 405 (the collection route registers
# only get+post; crates/apiserver/src/app.rs), and the discovery contract
# advertises NO deletecollection verb for configmaps (golden/
# discovery-core-v1.json: get/list/watch/create/update/patch/delete).
# IMPLEMENTED semantics: wire behavior and advertised verbs are CONSISTENT;
# the upstream delta (k8s serves DELETE on collections) is a v1 scope gap,
# not a wire inconsistency, so this is pinned rather than ledgered as (a).
check_method G51 "DELETE" "/api/v1/namespaces/default/configmaps" "405" "DELETE on a collection path is rejected (405; deletecollection not an advertised verb)"

# --- G52: ?labelSelector= is silently ignored ------------------------------
# ACTUAL: two ConfigMaps with distinct labels; the filtered list returns BOTH
# -> the selector is not applied. Code reading: ListParams (crates/apiserver/
# src/collection.rs) deserializes ONLY watch/limit/continue/resourceVersion,
# and serde drops unknown query keys -- so labelSelector never reaches the
# filter. Upstream k8s filters server-side (and errors on a bad selector).
# FINDING (a): silent ignore of a wire parameter is the worst failure mode
# (a client cannot tell filtering from a coincidental result set).
NG_CM1='{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"rt-ng-cm-a","namespace":"default","labels":{"app":"rt-ng-a"}},"data":{"k":"v"}}'
NG_CM2='{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"rt-ng-cm-b","namespace":"default","labels":{"app":"rt-ng-b"}},"data":{"k":"v"}}'
NG_A="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$NG_CM1" "$BASE/api/v1/namespaces/default/configmaps" 2>/dev/null || true)"
NG_B="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$NG_CM2" "$BASE/api/v1/namespaces/default/configmaps" 2>/dev/null || true)"
NG_LS="$(curl -s --max-time 5 "$BASE/api/v1/namespaces/default/configmaps?labelSelector=app%3Drt-ng-a" 2>/dev/null || true)"
NG_NAMES="$(printf '%s' "$NG_LS" | python3 -c 'import json,sys
try:
    d=json.load(sys.stdin)
    print(",".join(sorted(i["metadata"]["name"] for i in d.get("items",[]) if str(i.get("metadata",{}).get("name","")).startswith("rt-ng-cm-"))))
except Exception:
    print("PARSE-ERROR")' 2>/dev/null || echo PARSE-ERROR)"
if [[ "$NG_A" == "201" && "$NG_B" == "201" && "$NG_NAMES" == "rt-ng-cm-a,rt-ng-cm-b" ]]; then
  ok "G52  GET configmaps?labelSelector=app%3Drt-ng-a -> BOTH ConfigMaps (selector silently ignored; FINDING (a): ListParams never parses it)"
  PASS=$((PASS+1))
else
  echo "FAIL G52  labelSelector probe (a=$NG_A b=$NG_B names=$NG_NAMES; pinned actual was: selector ignored, both items)"
  FAIL=$((FAIL+1))
fi
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE/api/v1/namespaces/default/configmaps/rt-ng-cm-a" 2>/dev/null || true
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE/api/v1/namespaces/default/configmaps/rt-ng-cm-b" 2>/dev/null || true

# --- G53: https --server is rejected fast (Q21) ----------------------------
# LOCKED (Q21): the kubectl-compatible client is plain-HTTP in v1 (no TLS,
# no auth); crates/kubectl/src/http.rs::HttpClient::parse hard-errors on
# https:// BEFORE any connection. Must fail FAST (rc!=0, no hang), so the
# timeout below must never fire (rc 124 = hang = conformance break).
# (`get agents` is used because core `pods` is outside the v1 get-verb
# resource table (Q30/Q35 scope); the https reject happens after resource
# resolution and before any I/O.)
ln -sf "$BIN" "$NG_TMP/kubectl"
NG_T0="$(date +%s)"
NG_ERR="$(timeout 10 "$NG_TMP/kubectl" get agents --server "https://127.0.0.1:$PORT" 2>&1 >/dev/null)" && NG_RC=0 || NG_RC=$?
NG_T1="$(date +%s)"
if [[ "$NG_RC" -ne 0 && "$NG_RC" -ne 124 ]] && printf '%s' "$NG_ERR" | grep -q 'https not supported in v1 (Q21)'; then
  ok "G53  kubectl --server https://... -> rc=$NG_RC in $((NG_T1-NG_T0))s, stderr: $NG_ERR  (LOCKED Q21 plain-HTTP client; fails fast, no hang)"
  PASS=$((PASS+1))
else
  echo "FAIL G53  https reject (rc=$NG_RC stderr=$NG_ERR; expected fast rc!=0 with 'https not supported in v1 (Q21)')"
  FAIL=$((FAIL+1))
fi

# --- G54: sqlite:// datastore is refused (Q33) ------------------------------
# LOCKED (Q33): the durable option is rdb:// (RocksDB); the Sprint-19
# libsql/SQLite backend was REMOVED, so sqlite:// must refuse to serve:
# non-zero exit AND the process never reaches "discovery listening"
# (cli/src/runtime.rs::open_store -> Dsn::Unsupported -> fatal before the
# apiserver binds).
NG_DD2="$(mktemp -d)"; NG_LOG2="$(mktemp)"; NG_PORT2="$(pick_port)"
timeout 10 "$BIN" server --data-dir "$NG_DD2" --datastore-endpoint "sqlite://$NG_DD2/x.db" \
  --bind-address 127.0.0.1 --https-listen-port "$NG_PORT2" >"$NG_LOG2" 2>&1 && NG_RC2=0 || NG_RC2=$?
if [[ "$NG_RC2" -ne 0 && "$NG_RC2" -ne 124 ]] \
   && ! grep -q "discovery listening" "$NG_LOG2" 2>/dev/null \
   && grep -q "unsupported --datastore-endpoint" "$NG_LOG2" 2>/dev/null; then
  ok "G54  server --datastore-endpoint sqlite://... -> rc=$NG_RC2, never 'discovery listening', refuses with 'unsupported --datastore-endpoint'  (LOCKED Q33: rdb:// is the only durable option)"
  PASS=$((PASS+1))
else
  echo "FAIL G54  sqlite:// was not refused cleanly (rc=$NG_RC2 log=$(tr '\n' '|' <"$NG_LOG2" 2>/dev/null | head -c 200))"
  FAIL=$((FAIL+1))
fi
rm -rf "$NG_DD2" "$NG_LOG2" 2>/dev/null || true

# --- G55: pods/binding: first bind 201, duplicate 409 -----------------------
# IMPLEMENTED semantics (crates/apiserver/src/binding.rs): a bind writes
# spec.nodeName in-process (Q19) and echoes the Binding with 201; a second
# bind on an already-assigned pod -> 409 Conflict ("already assigned to
# node"). The throwaway pod carries schedulerName=golden-none so the
# built-in scheduler (scheduler/src/plugin.rs::is_pending only claims
# empty-or-default-schedulerName pods) cannot race the first bind.
NG_BP='{"apiVersion":"v1","kind":"Pod","metadata":{"name":"rt-ng-bind","namespace":"default"},"spec":{"schedulerName":"golden-none","containers":[{"name":"c","image":"i"}]}}'
NG_BPOD="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$NG_BP" "$BASE/api/v1/namespaces/default/pods" 2>/dev/null || true)"
NG_B1="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary '{"apiVersion":"v1","kind":"Binding","metadata":{"name":"rt-ng-bind"},"target":{"kind":"Node","name":"rt-node-a"}}' "$BASE/api/v1/namespaces/default/pods/rt-ng-bind/binding" 2>/dev/null || true)"
NG_B2="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary '{"apiVersion":"v1","kind":"Binding","metadata":{"name":"rt-ng-bind"},"target":{"kind":"Node","name":"rt-node-b"}}' "$BASE/api/v1/namespaces/default/pods/rt-ng-bind/binding" 2>/dev/null || true)"
if [[ "$NG_BPOD" == "201" && "$NG_B1" == "201" && "$NG_B2" == "409" ]]; then
  ok "G55  pods/binding: create -> 201 then duplicate -> 409 Conflict  (IMPLEMENTED binding.rs semantics, Q19 in-process bind)"
  PASS=$((PASS+1))
else
  echo "FAIL G55  binding probe (pod=$NG_BPOD first=$NG_B1 dup=$NG_B2; expected 201/201/409)"
  FAIL=$((FAIL+1))
fi
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE/api/v1/namespaces/default/pods/rt-ng-bind" 2>/dev/null || true

# --- teardown hygiene: nothing this suite leaves behind ---
rm -rf "$NG_TMP" 2>/dev/null || true
