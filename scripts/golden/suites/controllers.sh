# Controllers suite (G16-G21): T3.1a Deployment/Endpoints convergence;
# T3.1b rolling update + rollout status, StatefulSet, DaemonSet, GC/drain.
# Sourced by golden-conformance.sh; runs in the main script's shell.
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
