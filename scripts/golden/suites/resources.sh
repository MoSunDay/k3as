# Resource-surface suite (A1-A3): table-driven REST CRUD / watch / SSA sweep
# across the served kinds that had zero or weak wire coverage until now --
# Secret, Event (no producer yet: hand-created), PersistentVolumeClaim,
# Lease, IngressClass (the ONLY cluster-scoped kind never yet exercised over
# the wire), Ingress, Service and Endpoints -- plus the A2 watch sweep
# (incl. the cluster-scoped node path and rv=0 history replay) and the A3
# SSA sweep by group representative (Secret/Deployment/Ingress/Agent).
# Sourced by golden-conformance.sh; runs in the main script's shell
# (helpers: check_body/check_status/check_method/ok, counters, $BASE, $BIN).
# Owned by plan item: A1+A2+A3. Check-ID range reserved: G32-G45. Cap: 400.
#
# Safety: every curl is guarded (`|| true`, `2>/dev/null || true`) so one
# refused request can never abort the golden run; watch streams are bounded
# by --max-time plus a background kill (curl exit 28 on idle is the expected
# stream-idle outcome, never an error). Q22 is honoured: PVC legs assert
# CRUD/watch only -- with no binder its status stays Pending (locked).

RES_TMPD="$(mktemp -d)"
RES_CLEANUP=()

# --- suite-private helpers (RES_BAD is the per-check failure accumulator) ---
res_leg() { # <desc> <method> <path> <want> [content-type] [body-file]
  local desc="$1" method="$2" path="$3" want="$4" ct="${5:-}" bf="${6:-}" code
  local extra=()
  [[ -n "$ct" ]] && extra+=(-H "Content-Type: $ct")
  [[ -n "$bf" ]] && extra+=(--data-binary "@$bf")
  code="$(curl -s -o /dev/null -w '%{http_code}' -X "$method" "${extra[@]}" "$BASE$path" 2>/dev/null || true)"
  [[ "$code" == "$want" ]] || RES_BAD+=" $desc=$code(want $want)"
  return 0
}

# GET a path and require a literal (grep -F) substring in the body.
res_has() { # <desc> <path> <fixed-substring>
  local body
  body="$(curl -s "$BASE$2" 2>/dev/null || true)"
  printf '%s' "$body" | grep -qF -- "$3" || RES_BAD+=" $1-missing"
  return 0
}

# Current metadata.resourceVersion of an object ("0" when unreadable).
res_rv() { # <item-path>
  curl -s "$BASE$1" 2>/dev/null | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("metadata",{}).get("resourceVersion","0"))
except Exception: print("0")' 2>/dev/null || echo 0
}

# Rewrite a JSON body file with a chosen metadata.resourceVersion (CAS legs).
res_with_rv() { # <in> <out> <rv>
  python3 -c 'import json,sys
v=json.load(open(sys.argv[1]))
v.setdefault("metadata",{})["resourceVersion"]=sys.argv[3]
open(sys.argv[2],"w").write(json.dumps(v))' "$1" "$2" "$3" 2>/dev/null || true
  return 0
}

# Bounded watch stream (G15/G12 style, backgrounded): open, wait until the
# response HEADERS arrive (= the server subscribed before any later write,
# so no sleep guessing), then poll the captured body for literal markers.
res_watch_start() { # <out-file> <path-with-query>
  RES_WOUT="$1"; RES_WHDR="$1.hdr"; : >"$RES_WOUT"; : >"$RES_WHDR"
  curl -sN --max-time 8 -D "$RES_WHDR" -o "$RES_WOUT" "$BASE$2" 2>/dev/null &
  RES_WPID=$!
  return 0
}
res_watch_stop() {
  [[ -n "${RES_WPID:-}" ]] && { kill "$RES_WPID" 2>/dev/null || true; wait "$RES_WPID" 2>/dev/null || true; }
  RES_WPID=""
  return 0
}
res_watch_ready() {
  local i
  for i in $(seq 1 40); do
    grep -q '^HTTP/' "$RES_WHDR" 2>/dev/null && return 0
    sleep 0.05
  done
  return 1
}
res_watch_wait() { # <fixed1> [fixed2] -> 0 once both are present in $RES_WOUT
  local p1="$1" p2="${2:-}" i body
  for i in $(seq 1 30); do
    body="$(cat "$RES_WOUT" 2>/dev/null || true)"
    if printf '%s' "$body" | grep -qF -- "$p1" \
       && { [[ -z "$p2" ]] || printf '%s' "$body" | grep -qF -- "$p2"; }; then
      res_watch_stop; return 0
    fi
    sleep 0.15
  done
  res_watch_stop; return 1
}

# --- A1: full REST CRUD sweep (G32-G39), one compound check per kind --------
# Per kind: POST->201, GET->200+marker, LIST->200+item+listKind, PUT with a
# stale resourceVersion->409 (CAS), PUT with the fresh rv->200, JSON merge
# PATCH->200 + annotation read-back, DELETE->200, GET->404.
# Row: id|group|version|plural|scope|namespace|name|marker|listKind|body-var
B_SECRET='{"apiVersion":"v1","kind":"Secret","metadata":{"name":"golden-res-secret","namespace":"default"},"type":"Opaque","data":{"k":"dGVzdA=="}}'
B_EVENT='{"apiVersion":"v1","kind":"Event","metadata":{"name":"golden-res-event","namespace":"default"},"involvedObject":{"kind":"Pod","name":"golden-res-event-src","namespace":"default","apiVersion":"v1"},"reason":"GoldenReason","message":"golden resources sweep","type":"Normal","count":1,"source":{"component":"golden-conformance"}}'
B_PVC='{"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":"golden-res-pvc","namespace":"default"},"spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Gi"}}}}'
B_LEASE='{"apiVersion":"coordination.k8s.io/v1","kind":"Lease","metadata":{"name":"golden-res-lease","namespace":"default"},"spec":{"holderIdentity":"golden"}}'
B_INGCLASS='{"apiVersion":"networking.k8s.io/v1","kind":"IngressClass","metadata":{"name":"golden-res-ic"},"spec":{"controller":"golden.example.com/ingress"}}'
B_ING='{"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":"golden-res-ing","namespace":"default"},"spec":{"ingressClassName":"golden-res-ic","rules":[{"host":"golden-res.example.com","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"golden-res-svc","port":{"number":80}}}}]}}]}}'
B_SVC='{"apiVersion":"v1","kind":"Service","metadata":{"name":"golden-res-svc","namespace":"default"},"spec":{"ports":[{"port":80,"protocol":"TCP"}]}}'
B_EPS='{"apiVersion":"v1","kind":"Endpoints","metadata":{"name":"golden-res-ep","namespace":"default"},"subsets":[{"addresses":[{"ip":"10.0.0.42"}],"ports":[{"port":80,"protocol":"TCP"}]}]}'
RES_ROWS=(
  "G32|core|v1|secrets|namespaced|default|golden-res-secret|dGVzdA==|SecretList|B_SECRET"
  "G33|core|v1|events|namespaced|default|golden-res-event|\"reason\":\"GoldenReason\"|EventList|B_EVENT"
  "G34|core|v1|persistentvolumeclaims|namespaced|default|golden-res-pvc|ReadWriteOnce|PersistentVolumeClaimList|B_PVC"
  "G35|coordination.k8s.io|v1|leases|namespaced|default|golden-res-lease|\"holderIdentity\":\"golden\"|LeaseList|B_LEASE"
  "G36|networking.k8s.io|v1|ingressclasses|cluster|-|golden-res-ic|golden.example.com/ingress|IngressClassList|B_INGCLASS"
  "G37|networking.k8s.io|v1|ingresses|namespaced|default|golden-res-ing|golden-res.example.com|IngressList|B_ING"
  "G38|core|v1|services|namespaced|default|golden-res-svc|\"port\":80|ServiceList|B_SVC"
  "G39|core|v1|endpoints|namespaced|default|golden-res-ep|10.0.0.42|EndpointsList|B_EPS"
)
printf '%s' '{"metadata":{"annotations":{"golden-resources/patch":"yes"}}}' > "$RES_TMPD/patch.json"
for row in "${RES_ROWS[@]}"; do
  IFS='|' read -r G_ID G_GROUP G_VER G_PLURAL G_SCOPE G_NS G_NAME G_MARK G_LKIND G_BVAR <<<"$row"
  [[ "$G_GROUP" == "core" ]] && G_BASE="/api/$G_VER" || G_BASE="/apis/$G_GROUP/$G_VER"
  if [[ "$G_SCOPE" == "namespaced" ]]; then
    G_COLL="$G_BASE/namespaces/$G_NS/$G_PLURAL"
  else
    G_COLL="$G_BASE/$G_PLURAL"
  fi
  G_ITEM="$G_COLL/$G_NAME"
  RES_CLEANUP+=("$G_ITEM")
  printf '%s' "${!G_BVAR}" > "$RES_TMPD/a1.json"
  RES_BAD=""
  res_leg "post" "POST" "$G_COLL" 201 "application/json" "$RES_TMPD/a1.json"
  res_has "get" "$G_ITEM" "$G_MARK"
  res_has "list" "$G_COLL" "$G_NAME"
  res_has "listkind" "$G_COLL" "\"kind\":\"$G_LKIND\""
  RES_RV="$(res_rv "$G_ITEM")"
  if [[ "$RES_RV" =~ ^[0-9]+$ ]] && [[ "$RES_RV" -ge 1 ]]; then
    res_with_rv "$RES_TMPD/a1.json" "$RES_TMPD/stale.json" "$(( RES_RV > 1 ? RES_RV - 1 : RES_RV + 1 ))"
    res_leg "put-stale" "PUT" "$G_ITEM" 409 "application/json" "$RES_TMPD/stale.json"
    res_with_rv "$RES_TMPD/a1.json" "$RES_TMPD/fresh.json" "$RES_RV"
    res_leg "put-fresh" "PUT" "$G_ITEM" 200 "application/json" "$RES_TMPD/fresh.json"
  else
    RES_BAD+=" rv-unreadable"
  fi
  res_leg "patch" "PATCH" "$G_ITEM" 200 "application/merge-patch+json" "$RES_TMPD/patch.json"
  res_has "patched" "$G_ITEM" '"golden-resources/patch":"yes"'
  res_leg "delete" "DELETE" "$G_ITEM" 200
  res_leg "gone" "GET" "$G_ITEM" 404
  if [[ -z "$RES_BAD" ]]; then
    ok "$G_ID  CRUD sweep $G_PLURAL ($G_SCOPE, $G_GROUP/$G_VER)  (A1 resources)"
    PASS=$((PASS+1))
  else
    echo "FAIL $G_ID  CRUD sweep $G_PLURAL ($G_SCOPE, $G_GROUP/$G_VER)  --$RES_BAD"
    FAIL=$((FAIL+1))
  fi
done

# --- A2: watch sweep (G40 ADDED / G41 DELETED / G45 stream-status) ----------
# Row: kind|collection-path|name|body-var
W_POD='{"apiVersion":"v1","kind":"Pod","metadata":{"name":"golden-res-w-pod","namespace":"default"},"spec":{"containers":[{"name":"c","image":"nginx:1.29"}]}}'
W_DEP='{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"golden-res-w-dep","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"golden-res-w-dep"}},"template":{"metadata":{"labels":{"app":"golden-res-w-dep"}},"spec":{"containers":[{"name":"c","image":"nginx:1.29"}]}}}}'
W_RS='{"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"golden-res-w-rs","namespace":"default"},"spec":{"replicas":0,"selector":{"matchLabels":{"app":"golden-res-w-rs"}},"template":{"metadata":{"labels":{"app":"golden-res-w-rs"}},"spec":{"containers":[{"name":"c","image":"nginx:1.29"}]}}}}'
W_NODE='{"apiVersion":"v1","kind":"Node","metadata":{"name":"golden-res-w-node"}}'
W_SVC='{"apiVersion":"v1","kind":"Service","metadata":{"name":"golden-res-w-svc","namespace":"default"},"spec":{"ports":[{"port":80,"protocol":"TCP"}]}}'
W_EPS='{"apiVersion":"v1","kind":"Endpoints","metadata":{"name":"golden-res-w-ep","namespace":"default"},"subsets":[{"addresses":[{"ip":"10.0.0.43"}],"ports":[{"port":80,"protocol":"TCP"}]}]}'
W_WF='{"apiVersion":"argoproj.io/v1alpha1","kind":"Workflow","metadata":{"name":"golden-res-w-wf","namespace":"default"},"spec":{"entrypoint":"main","templates":[{"name":"main","container":{"name":"main","image":"docker/whalesay"}}]}}'
W_ING='{"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":"golden-res-w-ing","namespace":"default"},"spec":{"rules":[{"host":"golden-res-w.example.com","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"golden-res-w-svc","port":{"number":80}}}}]}}]}}'
RES_WROWS=(
  "Pod|/api/v1/namespaces/default/pods|golden-res-w-pod|W_POD"
  "Deployment|/apis/apps/v1/namespaces/default/deployments|golden-res-w-dep|W_DEP"
  "ReplicaSet|/apis/apps/v1/namespaces/default/replicasets|golden-res-w-rs|W_RS"
  "Node|/api/v1/nodes|golden-res-w-node|W_NODE"
  "Service|/api/v1/namespaces/default/services|golden-res-w-svc|W_SVC"
  "Endpoints|/api/v1/namespaces/default/endpoints|golden-res-w-ep|W_EPS"
  "Workflow|/apis/argoproj.io/v1alpha1/namespaces/default/workflows|golden-res-w-wf|W_WF"
  "Ingress|/apis/networking.k8s.io/v1/namespaces/default/ingresses|golden-res-w-ing|W_ING"
)
RES_W_ADD=""; RES_W_DEL=""; RES_W_HTTP=""
for row in "${RES_WROWS[@]}"; do
  IFS='|' read -r W_KIND W_COLL W_NAME W_BVAR <<<"$row"
  W_ITEM="$W_COLL/$W_NAME"
  RES_CLEANUP+=("$W_ITEM")
  printf '%s' "${!W_BVAR}" > "$RES_TMPD/a2.json"
  # ADDED leg: subscribe BEFORE the create so the live stream cannot miss it.
  res_watch_start "$RES_TMPD/a2-add.out" "$W_COLL?watch=1"
  if res_watch_ready && grep -q '^HTTP/.* 200' "$RES_WHDR" 2>/dev/null; then :; else
    RES_W_HTTP+="$W_KIND "
  fi
  W_CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$RES_TMPD/a2.json" "$BASE$W_COLL" 2>/dev/null || true)"
  res_watch_wait '"type":"ADDED"' "\"name\":\"$W_NAME\"" || RES_W_ADD+="$W_KIND "
  [[ "$W_CODE" == "201" ]] || RES_W_ADD+="$W_KIND(create=$W_CODE) "
  # DELETED leg: replay from the create revision (events AFTER it), so the
  # delete is delivered deterministically even on an otherwise idle stream.
  W_RV="$(res_rv "$W_ITEM")"
  curl -s -o /dev/null -X DELETE "$BASE$W_ITEM" 2>/dev/null || true
  res_watch_start "$RES_TMPD/a2-del.out" "$W_COLL?watch=1&resourceVersion=$W_RV"
  res_watch_ready || true
  res_watch_wait '"type":"DELETED"' "\"name\":\"$W_NAME\"" || RES_W_DEL+="$W_KIND "
done
if [[ -z "$RES_W_ADD" ]]; then
  ok "G40  watch ADDED sweep over pod/deployment/rs/node/service/endpoints/workflow/ingress  (A2 resources)"
  PASS=$((PASS+1))
else
  echo "FAIL G40  watch ADDED sweep, kinds without ADDED: $RES_W_ADD"
  FAIL=$((FAIL+1))
fi
if [[ -z "$RES_W_DEL" ]]; then
  ok "G41  watch DELETED sweep at resourceVersion=<create> over the same 8 kinds  (A2 resources)"
  PASS=$((PASS+1))
else
  echo "FAIL G41  watch DELETED sweep, kinds without DELETED: $RES_W_DEL"
  FAIL=$((FAIL+1))
fi
if [[ -z "$RES_W_HTTP" ]]; then
  ok "G45  every watch stream opened with HTTP 200 (incl. cluster-scoped /api/v1/nodes)  (A2 resources)"
  PASS=$((PASS+1))
else
  echo "FAIL G45  watch streams without a 200 status: $RES_W_HTTP"
  FAIL=$((FAIL+1))
fi

# --- G42: rv=0 history replay for a core and a grouped kind (T2.2 contract) -
B_REPLAY_SEC='{"apiVersion":"v1","kind":"Secret","metadata":{"name":"golden-res-replay","namespace":"default"},"type":"Opaque","data":{"k":"dGVzdA=="}}'
B_REPLAY_ING='{"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":"golden-res-replay","namespace":"default"},"spec":{"rules":[{"host":"golden-res-replay.example.com","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"golden-res-svc","port":{"number":80}}}}]}}]}}'
RES_R_BAD=""
for pair in "Secret|/api/v1/namespaces/default/secrets|B_REPLAY_SEC" \
            "Ingress|/apis/networking.k8s.io/v1/namespaces/default/ingresses|B_REPLAY_ING"; do
  IFS='|' read -r R_KIND R_COLL R_BVAR <<<"$pair"
  printf '%s' "${!R_BVAR}" > "$RES_TMPD/replay.json"
  curl -s -o /dev/null -X POST -H "Content-Type: application/json" --data-binary "@$RES_TMPD/replay.json" "$BASE$R_COLL" 2>/dev/null || true
  RES_CLEANUP+=("$R_COLL/golden-res-replay")
  R_OK=1
  res_watch_start "$RES_TMPD/replay.out" "$R_COLL?watch=1&resourceVersion=0"
  res_watch_ready || R_OK=0
  res_watch_wait '"type":"ADDED"' '"name":"golden-res-replay"' || R_OK=0
  [[ "$R_OK" -eq 1 ]] || RES_R_BAD+="$R_KIND "
done
if [[ -z "$RES_R_BAD" ]]; then
  ok "G42  watch resourceVersion=0 replays retained history (ADDED) for Secret + Ingress  (A2/T2.2)"
  PASS=$((PASS+1))
else
  echo "FAIL G42  rv=0 history replay missing ADDED for: $RES_R_BAD"
  FAIL=$((FAIL+1))
fi

# --- G43: generateName POSTs mint <prefix>+5-char suffix (G28 python style) --
B_GEN_SEC='{"apiVersion":"v1","kind":"Secret","metadata":{"generateName":"golden-res-gen-","namespace":"default"},"type":"Opaque","data":{"k":"dGVzdA=="}}'
B_GEN_EVT='{"apiVersion":"v1","kind":"Event","metadata":{"generateName":"golden-res-gen-","namespace":"default"},"involvedObject":{"kind":"Pod","name":"golden-res-event-src","namespace":"default"},"reason":"GoldenReason","type":"Normal"}'
B_GEN_ING='{"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"generateName":"golden-res-gen-","namespace":"default"},"spec":{"rules":[{"host":"golden-res-gen.example.com"}]}}'
RES_G_BAD=""
for pair in "Secret|/api/v1/namespaces/default/secrets|B_GEN_SEC" \
            "Event|/api/v1/namespaces/default/events|B_GEN_EVT" \
            "Ingress|/apis/networking.k8s.io/v1/namespaces/default/ingresses|B_GEN_ING"; do
  IFS='|' read -r N_KIND N_COLL N_BVAR <<<"$pair"
  printf '%s' "${!N_BVAR}" > "$RES_TMPD/gen.json"
  N_BODY="$(curl -s -w '
%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$RES_TMPD/gen.json" "$BASE$N_COLL" 2>/dev/null || true)"
  N_CODE="${N_BODY##*
}"
  N_JSON="${N_BODY%
*}"
  N_NAME="$(printf '%s' "$N_JSON" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("metadata",{}).get("name",""))
except Exception: pass' 2>/dev/null || true)"
  if [[ "$N_CODE" == "201" && "$N_NAME" == golden-res-gen-* && ${#N_NAME} -eq 20 ]] \
     && printf '%s' "$N_JSON" | grep -qF '"generateName":"golden-res-gen-"'; then :; else
    RES_G_BAD+="$N_KIND($N_CODE/${N_NAME:-none}) "
  fi
  [[ -n "$N_NAME" ]] && RES_CLEANUP+=("$N_COLL/$N_NAME")
done
if [[ -z "$RES_G_BAD" ]]; then
  ok "G43  generateName mints <prefix>+5-char suffix for Secret/Event/Ingress  (A1/T7.3 parity)"
  PASS=$((PASS+1))
else
  echo "FAIL G43  generateName sweep: $RES_G_BAD"
  FAIL=$((FAIL+1))
fi

# --- G44: A3 SSA sweep by group representative (apply-create 201, re-apply 200)
B_SSA_SEC='{"apiVersion":"v1","kind":"Secret","metadata":{"name":"golden-res-ssa","namespace":"default"},"type":"Opaque","data":{"k":"dGVzdA=="}}'
B_SSA_DEP='{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"golden-res-ssa","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"golden-res-ssa"}},"template":{"metadata":{"labels":{"app":"golden-res-ssa"}},"spec":{"containers":[{"name":"c","image":"nginx:1.29"}]}}}}'
B_SSA_ING='{"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":"golden-res-ssa","namespace":"default"},"spec":{"rules":[{"host":"golden-res-ssa.example.com","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"golden-res-svc","port":{"number":80}}}}]}}]}}'
B_SSA_AGT='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"golden-res-ssa","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-conformance"}}'
RES_S_BAD=""
for pair in "Secret|/api/v1/namespaces/default/secrets/golden-res-ssa|B_SSA_SEC" \
            "Deployment|/apis/apps/v1/namespaces/default/deployments/golden-res-ssa|B_SSA_DEP" \
            "Ingress|/apis/networking.k8s.io/v1/namespaces/default/ingresses/golden-res-ssa|B_SSA_ING" \
            "Agent|/apis/init-pro.io/v1/namespaces/default/agents/golden-res-ssa|B_SSA_AGT"; do
  IFS='|' read -r S_KIND S_PATH S_BVAR <<<"$pair"
  RES_CLEANUP+=("$S_PATH")
  printf '%s' "${!S_BVAR}" > "$RES_TMPD/ssa.json"
  S1="$(curl -s -o /dev/null -w '%{http_code}' -X PATCH -H "Content-Type: application/apply-patch+yaml" --data-binary "@$RES_TMPD/ssa.json" "$BASE$S_PATH?fieldManager=golden-resources" 2>/dev/null || true)"
  S2="$(curl -s -o /dev/null -w '%{http_code}' -X PATCH -H "Content-Type: application/apply-patch+yaml" --data-binary "@$RES_TMPD/ssa.json" "$BASE$S_PATH?fieldManager=golden-resources" 2>/dev/null || true)"
  [[ "$S1" == "201" && "$S2" == "200" ]] || RES_S_BAD+="$S_KIND($S1/$S2) "
done
if [[ -z "$RES_S_BAD" ]]; then
  ok "G44  SSA apply-create 201 + identical re-apply 200 for Secret/Deployment/Ingress/Agent  (A3 resources)"
  PASS=$((PASS+1))
else
  echo "FAIL G44  SSA sweep (create/re-apply): $RES_S_BAD"
  FAIL=$((FAIL+1))
fi

# Best-effort teardown: every object this suite touched is deleted (404 ok).
for p in "${RES_CLEANUP[@]}"; do
  curl -s -o /dev/null -X DELETE "$BASE$p" 2>/dev/null || true
done
rm -rf "$RES_TMPD"
