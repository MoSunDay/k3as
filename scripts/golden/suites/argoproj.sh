# T7.3 Scope A: the argoproj.io group is a first-class built-in. G27 pins the
# byte-stable discovery document (Workflow + CronWorkflow with wf/cwf/cronwf
# shortNames); G28 drives the full CRUD + SSA lifecycle over the generic
# apiserver routes -- typed 1:1 port of upstream Argo Workflows v4 types
# (JSON-only wire, Q10).

# G27: /apis/argoproj.io/v1alpha1 resource list is byte-stable.
check_body G27 "/apis/argoproj.io/v1alpha1" "$GOLDEN/discovery-argoproj-v1alpha1.json"

# G28: Workflow + CronWorkflow lifecycle -- POST create, GET read-back with
# camelCase fidelity, SSA apply-create then re-apply (idempotent 200), list
# contains the objects, DELETE then 404 (G13/G14 style for the SSA legs).
ARGO_NS="$BASE/apis/argoproj.io/v1alpha1/namespaces/default"
TMP_WF="$(mktemp)"
TMP_CWF="$(mktemp)"
TMP_WF_SSA="$(mktemp)"
printf '%s' '{"apiVersion":"argoproj.io/v1alpha1","kind":"Workflow","metadata":{"name":"golden-wf","namespace":"default"},"spec":{"entrypoint":"main","templates":[{"name":"main","container":{"name":"main","image":"docker/whalesay"}}]}}' > "$TMP_WF"
printf '%s' '{"apiVersion":"argoproj.io/v1alpha1","kind":"CronWorkflow","metadata":{"name":"golden-cwf","namespace":"default"},"spec":{"schedules":["*/5 * * * *"],"workflowSpec":{"entrypoint":"main","templates":[{"name":"main","container":{"name":"main","image":"docker/whalesay"}}]}}}' > "$TMP_CWF"
sed 's/"golden-wf"/"golden-wf-ssa"/' "$TMP_WF" > "$TMP_WF_SSA"

G28_CREATE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_WF" "$ARGO_NS/workflows")"
G28_CWF="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_CWF" "$ARGO_NS/cronworkflows")"
G28_GET="$(curl -s -o /dev/null -w '%{http_code}' "$ARGO_NS/workflows/golden-wf")"
G28_GETBODY="$(curl -s "$ARGO_NS/workflows/golden-wf")"
G28_SSA1="$(curl -s -o /dev/null -w '%{http_code}' -X PATCH -H "Content-Type: application/apply-patch+yaml" --data-binary "@$TMP_WF_SSA" "$ARGO_NS/workflows/golden-wf-ssa?fieldManager=golden-test")"
G28_SSA2="$(curl -s -o /dev/null -w '%{http_code}' -X PATCH -H "Content-Type: application/apply-patch+yaml" --data-binary "@$TMP_WF_SSA" "$ARGO_NS/workflows/golden-wf-ssa?fieldManager=golden-test")"
G28_LIST="$(curl -s "$ARGO_NS/workflows")"
G28_DEL="$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$ARGO_NS/workflows/golden-wf")"
G28_404="$(curl -s -o /dev/null -w '%{http_code}' "$ARGO_NS/workflows/golden-wf")"
G28_CWF_DEL="$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$ARGO_NS/cronworkflows/golden-cwf")"
# generateName-only POST: server mints <prefix>+5-char suffix (upstream parity).
TMP_WF_GEN="$(mktemp)"
printf '%s' '{"apiVersion":"argoproj.io/v1alpha1","kind":"Workflow","metadata":{"generateName":"golden-gen-","namespace":"default"},"spec":{"entrypoint":"main","templates":[{"name":"main","container":{"name":"main","image":"docker/whalesay"}}]}}' > "$TMP_WF_GEN"
G28_GEN="$(curl -s -X POST -H "Content-Type: application/json" --data-binary "@$TMP_WF_GEN" "$ARGO_NS/workflows")"
rm -f "$TMP_WF_GEN"

G28_GEN_OK=0
G28_GEN_NAME="$(printf '%s' "$G28_GEN" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("metadata",{}).get("name",""))
except Exception: pass' 2>/dev/null || true)"
if [[ "$G28_GEN_NAME" == golden-gen-* && ${#G28_GEN_NAME} -eq 16 ]] \
   && printf '%s' "$G28_GEN" | grep -q '"generateName":"golden-gen-"'; then
  G28_GEN_OK=1
fi
if [[ "$G28_CREATE" == "201" && "$G28_CWF" == "201" && "$G28_GET" == "200" \
      && "$G28_SSA1" == "201" && "$G28_SSA2" == "200" && "$G28_DEL" == "200" \
      && "$G28_404" == "404" && "$G28_CWF_DEL" == "200" && "$G28_GEN_OK" -eq 1 ]] \
   && printf '%s' "$G28_GETBODY" | grep -q '"entrypoint":"main"' \
   && printf '%s' "$G28_GETBODY" | grep -q '"templates":\[' \
   && printf '%s' "$G28_LIST" | grep -q 'golden-wf-ssa'; then
  ok "G28  workflow + cronWorkflow CRUD/SSA lifecycle over argoproj.io/v1alpha1  (T7.3 Scope A)"
  PASS=$((PASS+1))
else
  echo "FAIL G28  argoproj lifecycle (create=$G28_CREATE cwf=$G28_CWF get=$G28_GET ssa1=$G28_SSA1 ssa2=$G28_SSA2 del=$G28_DEL gone=$G28_404 cwf-del=$G28_CWF_DEL gen=$G28_GEN_OK/$G28_GEN_NAME)"
  echo "    get-body: $G28_GETBODY" >&2
  echo "    list: $G28_LIST" >&2
  FAIL=$((FAIL+1))
fi
rm -f "$TMP_WF" "$TMP_CWF" "$TMP_WF_SSA"
