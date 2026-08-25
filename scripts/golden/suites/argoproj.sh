# T7.3 Scope A: the argoproj.io group is a first-class built-in. G27 pins the
# byte-stable discovery document (Workflow + CronWorkflow with wf/cwf/cronwf
# shortNames); G28 drives the full CRUD + SSA lifecycle over the generic
# apiserver routes -- typed 1:1 port of upstream Argo Workflows v4 types
# (JSON-only wire, Q10).
# T7.3 Scope B (G29): a two-step Workflow runs to Succeeded over the real
# agent/containerd stack with the Q27 airgap wfstep image.

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

# --- T7.3 Scope B: a two-step Workflow runs to Succeeded end-to-end (G29) ---
# The workflow controller (in-process, Q19) drives real step pods over the
# full agent/containerd stack: Workflow -> step-a pod -> exit 0 -> group
# gate -> step-b pod -> exit 0 -> status.nodes reports the root Steps node
# Succeeded plus one Succeeded Pod node per step template. The step workload
# is the Q27 airgap one-shot `wfstep` image (scripts/build-workflow-image.sh)
# — pause/echo never exit, but a step's exit code IS its verdict
# (restartPolicy Never). Same SKIP gates as G25 (Q25 SKIP-not-fail): no
# vendored containerd bundle, or no cc for the airgap images. G27/G28 above
# used the orchestrator's shared main server; G29 boots its own SRV2 + agent
# exactly like G25 and hands them back cleared.
if [[ -z "$G24_VENDOR_BIN" ]]; then
  echo "SKIP G29 workflow end-to-end (no vendored containerd)"
elif ! command -v cc >/dev/null 2>&1; then
  echo "SKIP G29 workflow end-to-end (no cc for the airgap step image, Q27)"
else
  G29TMP="$(mktemp -d)"; G29SRV="$(mktemp)"; G29AG="$(mktemp)"
  G29_PORT="$(pick_port)"
  "$BIN" server --data-dir "$G29TMP/srv" --bind-address 127.0.0.1 --https-listen-port "$G29_PORT" >"$G29SRV" 2>&1 &
  SRV2=$!
  for _ in $(seq 1 60); do grep -q "discovery listening" "$G29SRV" && break; sleep 0.1; done
  BASE29="http://127.0.0.1:${G29_PORT}"
  G29_PAUSE_REF="init-pro.local/pause:0.1"
  G29_WF_REF="init-pro.local/wfstep:0.1"
  "$SCRIPT_DIR/build-pause-image.sh" "$G29TMP/pause.tar" "$G29_PAUSE_REF" >/dev/null 2>&1 || true
  "$SCRIPT_DIR/build-workflow-image.sh" "$G29TMP/wfstep.tar" "$G29_WF_REF" >/dev/null 2>&1 || true
  INIT_PRO_SANDBOX_IMAGE="$G29_PAUSE_REF" "$BIN" agent --data-dir "$G29TMP/ag" --token dev \
      --server "$BASE29" --node-name g29-node >"$G29AG" 2>&1 &
  AGENT_PID=$!
  for _ in $(seq 1 300); do grep -q "containerd healthy" "$G29AG" && break; sleep 0.1; done
  INIT_PRO_DATA_DIR="$G29TMP/ag" "$BIN" ctr -n k8s.io images import "$G29TMP/pause.tar" >/dev/null 2>&1 || true
  INIT_PRO_DATA_DIR="$G29TMP/ag" "$BIN" ctr -n k8s.io images import "$G29TMP/wfstep.tar" >/dev/null 2>&1 || true
  # Two-step workflow: steps is the OUTER list of single-step lists (groups
  # gate sequentially); no parameter substitution (v1 controller). The step
  # pods run the one-shot wfstep binary -> exit 0 -> restartPolicy Never
  # marks the pod Succeeded.
  G29_WF="$G29TMP/two-step.yaml"
  cat > "$G29_WF" <<'YAML'
apiVersion: argoproj.io/v1alpha1
kind: Workflow
metadata:
  generateName: two-step-
  namespace: default
spec:
  entrypoint: main
  templates:
  - name: main
    steps:
    - - name: step-a
        template: say-a
    - - name: step-b
        template: say-b
  - name: say-a
    container:
      name: main
      image: init-pro.local/wfstep:0.1
      command: [wfstep]
      args: [step-a]
  - name: say-b
    container:
      name: main
      image: init-pro.local/wfstep:0.1
      command: [wfstep]
      args: [step-b]
YAML
  # kubectl via argv[0] multicall symlink (T0.1 contract, G18 pattern).
  G29_KCTL="$(mktemp -d)"
  ln -sf "$(readlink -f "$BIN")" "$G29_KCTL/kubectl"
  G29_OUT="$("$G29_KCTL/kubectl" apply --server "$BASE29" -f "$G29_WF" 2>&1)" || true
  G29_NAME="$(printf '%s' "$G29_OUT" | sed -n 's/^Workflow\/\(two-step-[a-z0-9]*\) created$/\1/p')"
  if [[ -z "$G29_NAME" ]]; then
    echo "FAIL G29  kubectl apply did not mint a two-step-* Workflow (out: $G29_OUT)"
    echo "    server: $G29SRV agent: $G29AG" >&2
    FAIL=$((FAIL+1))
  else
    G29_PHASE=""
    for _ in $(seq 1 240); do
      curl -s --max-time 5 -o "$G29TMP/wf.json" \
        "$BASE29/apis/argoproj.io/v1alpha1/namespaces/default/workflows/$G29_NAME" || true
      G29_PHASE="$(python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1])).get("status",{}).get("phase",""))
except Exception: print("")' "$G29TMP/wf.json" 2>/dev/null || true)"
      [[ "$G29_PHASE" == "Succeeded" || "$G29_PHASE" == "Failed" || "$G29_PHASE" == "Error" ]] && break
      sleep 0.5
    done
    # Combined verdict over the final JSON: root Steps node Succeeded,
    # >=2 Pod nodes covering say-a/say-b both Succeeded, finishedAt set.
    G29_VERDICT="$(python3 - "$G29TMP/wf.json" <<'PY'
import json, sys
wf = json.load(open(sys.argv[1]))
st = wf.get("status", {})
name = wf.get("metadata", {}).get("name", "")
nodes = st.get("nodes", {})
bad = []
root = nodes.get(name)
if not root or root.get("type") != "Steps" or root.get("phase") != "Succeeded":
    bad.append(f"root node {name!r} not Steps/Succeeded")
pods = [n for n in nodes.values() if n.get("type") == "Pod"]
if len(pods) < 2:
    bad.append(f"expected >=2 Pod nodes, found {len(pods)}")
for tmpl in ("say-a", "say-b"):
    if not [n for n in pods if n.get("templateName") == tmpl and n.get("phase") == "Succeeded"]:
        bad.append(f"no Succeeded Pod node for template {tmpl}")
if not st.get("finishedAt"):
    bad.append("status.finishedAt empty")
print("; ".join(bad) if bad else "ok")
PY
)" || true
    if [[ "$G29_PHASE" == "Succeeded" && "$G29_VERDICT" == "ok" ]]; then
      ok "G29  two-step workflow Succeeded; node tree reports step pods  (workflow controller, T7.3 Scope B)"
      PASS=$((PASS+1))
    else
      echo "FAIL G29  workflow end-to-end (phase=${G29_PHASE:-?} verdict=${G29_VERDICT:-?}; server: $G29SRV agent: $G29AG)"
      cat "$G29TMP/wf.json" >&2 || true
      FAIL=$((FAIL+1))
    fi
  fi
  # Teardown mirroring G25: delete the owner first (GC cascades the step
  # pods; the kubelet then tears every sandbox down), so no containerd
  # mounts stay pinned under the data-dir when it is removed.
  if [[ -n "${G29_NAME:-}" ]]; then
    curl -s -o /dev/null -X DELETE \
      "$BASE29/apis/argoproj.io/v1alpha1/namespaces/default/workflows/$G29_NAME" || true
    for _ in $(seq 1 120); do
      G29_CNT="$(INIT_PRO_DATA_DIR="$G29TMP/ag" "$BIN" crictl pods -o json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null || echo 99)"
      [[ "$G29_CNT" == "0" ]] && break
      sleep 0.5
    done
  fi
  kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; SRV2=""
  kill -TERM "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; AGENT_PID=""
  rm -rf "$G29TMP" "$G29SRV" "$G29AG" "$G29_KCTL" 2>/dev/null || true
fi
