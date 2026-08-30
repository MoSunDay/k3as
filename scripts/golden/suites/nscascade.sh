# Namespace terminal-deletion cascade suite (B5): ns with running Deployment
# -> DELETE ns -> drain reaches real pods; DRAIN_KINDS closure (D1 fix).
# Sourced by golden-conformance.sh; runs in the main script's shell
# (helpers: check_body/check_status/check_method/ok, $BASE, $GOLDEN, $BIN).
# Owned by plan item: B5. Check-ID range reserved: G80-G82. Hard cap 400 lines.
#
# LIVE wire-level proof of the D1 fix (commit 03adb6e): DRAIN_KINDS grew
# 12 -> 18 because events, argoproj.io workflows+cronworkflows,
# init-pro.io agents+luarouters and networking.k8s.io ingresses used to
# survive namespace finalization as ownerless orphans (unreachable by the GC
# cascade, Q20). tests/gc.rs pins the fix in-process; G80-G82 pin it over the
# wire against a dedicated server+agent pair (suite-local data dirs + port,
# the G25 pattern): the 2-replica Deployment is RUNNING on real containerd
# when the namespace is deleted, so the drain must reach actual containers,
# not just etcd-like rows. Object shapes mirror tests/gc.rs exactly: the
# Workflow is terminal (phase Succeeded) and the CronWorkflow suspended with
# no schedule, so nothing re-fires children mid-drain; the Agent is created
# and (Q35 Phase A) may be scheduled but is never executed -- drained anyway.
# SKIPs exactly like G25 (no vendored containerd, or no cc for the Q27 airgap
# pause image). Every wait is a bounded convergence poll; no sleep masking.
# The pair uses the default in-memory store (Q17); durability (Q33 rdb://)
# is orthogonal to a drain and out of scope here.
if [[ -z "$G24_VENDOR_BIN" ]]; then
  echo "SKIP G80-G82 ns-cascade drain (no vendored containerd)"
elif ! command -v cc >/dev/null 2>&1; then
  echo "SKIP G80-G82 ns-cascade drain (no cc for the airgap pause image, Q27)"
else
  NSCTMP="$(mktemp -d)"; NSCSRV="$(mktemp)"; NSCAG="$(mktemp)"
  NSC_PORT="$(pick_port)"
  NS="golden-cascade"

  # Suite-local REST helpers: the global check_* helpers are pinned to $BASE
  # (the main run's server), and this suite owns its own pair (G25 style).
  nsc_code() { curl -s -o /dev/null -w '%{http_code}' "$@" 2>/dev/null || true; }
  nsc_post() { # <collection-path> <json> -> HTTP code
    printf '%s' "$2" >"$NSCTMP/body.json"
    nsc_code -X POST -H 'Content-Type: application/json' \
      --data-binary "@$NSCTMP/body.json" "$BASENSC$1"
  }
  nsc_items() { # <collection-path> -> item count ("ERR" when unparsable)
    curl -s "$BASENSC$1" 2>/dev/null | python3 -c 'import json,sys
try: print(len(json.load(sys.stdin).get("items",[])))
except Exception: print("ERR")' 2>/dev/null || echo ERR
  }

  # --- the pair: own server + own agent on real containerd ---------------
  "$BIN" server --data-dir "$NSCTMP/srv" --bind-address 127.0.0.1 \
      --https-listen-port "$NSC_PORT" >"$NSCSRV" 2>&1 &
  SRV2=$!
  for _ in $(seq 1 60); do grep -q "discovery listening" "$NSCSRV" && break; sleep 0.1; done
  BASENSC="http://127.0.0.1:${NSC_PORT}"
  PAUSE_REF="init-pro.local/pause:0.1"
  NSC_IMG="$NSCTMP/pause.tar"
  "$SCRIPT_DIR/build-pause-image.sh" "$NSC_IMG" "$PAUSE_REF" >/dev/null 2>&1 || true
  INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" "$BIN" agent --data-dir "$NSCTMP/ag" --token dev \
      --server "$BASENSC" --node-name nsc-node >"$NSCAG" 2>&1 &
  AGENT_PID=$!
  for _ in $(seq 1 300); do grep -q "containerd healthy" "$NSCAG" && break; sleep 0.1; done
  INIT_PRO_DATA_DIR="$NSCTMP/ag" "$BIN" ctr -n k8s.io images import "$NSC_IMG" >/dev/null 2>&1 || true

  # --- step 1: the namespace must carry the injected `kubernetes` finalizer
  NSC_NS_OK=0
  NSC_NS_CODE="$(nsc_post "/api/v1/namespaces" \
    '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"'"$NS"'"}}')"
  NSC_FIN="$(curl -s "$BASENSC/api/v1/namespaces/$NS" 2>/dev/null | python3 -c 'import json,sys
try:
  f=json.load(sys.stdin).get("metadata",{}).get("finalizers",[])
  print("|".join(f))
except Exception: pass' 2>/dev/null || true)"
  [[ "$NSC_NS_CODE" == "201" && "$NSC_FIN" == "kubernetes" ]] && NSC_NS_OK=1

  # --- step 2: populate via REST ----------------------------------------
  # Workload + supporting core kinds, then one object of EACH of the six
  # kinds that leaked before the fix (group/resource exactly as DRAIN_KINDS).
  NSC_DEP='{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"nsc-dep","namespace":"'"$NS"'"},"spec":{"replicas":2,"selector":{"matchLabels":{"app":"nsc"}},"template":{"metadata":{"labels":{"app":"nsc"}},"spec":{"containers":[{"name":"pause","image":"'"$PAUSE_REF"'"}]}}}}'
  NSC_SVC='{"apiVersion":"v1","kind":"Service","metadata":{"name":"nsc-svc","namespace":"'"$NS"'"},"spec":{"selector":{"app":"nsc"},"ports":[{"port":80,"protocol":"TCP"}]}}'
  NSC_CM='{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"nsc-cm","namespace":"'"$NS"'"},"data":{"greeting":"hello-from-nsc"}}'
  NSC_SEC='{"apiVersion":"v1","kind":"Secret","metadata":{"name":"nsc-secret","namespace":"'"$NS"'"},"type":"Opaque","data":{"k":"dGVzdA=="}}'
  NSC_EVT='{"apiVersion":"v1","kind":"Event","metadata":{"name":"nsc-ev","namespace":"'"$NS"'"},"involvedObject":{"kind":"Pod","name":"nsc-dep-x","namespace":"'"$NS"'","apiVersion":"v1"},"reason":"GoldenCascade","message":"nscascade sweep","type":"Normal","count":1,"source":{"component":"nscascade-suite"}}'
  # gc.rs terminal_workflow: status phase Succeeded -> the workflow controller
  # converges (early-returns) without creating child objects.
  NSC_WF='{"apiVersion":"argoproj.io/v1alpha1","kind":"Workflow","metadata":{"name":"nsc-wf","namespace":"'"$NS"'"},"status":{"phase":"Succeeded"}}'
  # gc.rs inert_cronworkflow: no schedule + suspend -> never fires children.
  NSC_CWF='{"apiVersion":"argoproj.io/v1alpha1","kind":"CronWorkflow","metadata":{"name":"nsc-cwf","namespace":"'"$NS"'"},"spec":{"suspend":true}}'
  # crates/api/src/agent.rs minimal spec (Q35 Phase A: scheduled, never run).
  NSC_AGT='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"nsc-ag","namespace":"'"$NS"'"},"spec":{"agent":"opencoder","prompt":"golden-cascade"}}'
  NSC_LR='{"apiVersion":"init-pro.io/v1","kind":"LuaRouter","metadata":{"name":"nsc-lr","namespace":"'"$NS"'"},"spec":{"routes":[]}}'
  NSC_ING='{"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":"nsc-ing","namespace":"'"$NS"'"},"spec":{"rules":[{"host":"nsc.example.com","http":{"paths":[{"path":"/","pathType":"Prefix","backend":{"service":{"name":"nsc-svc","port":{"number":80}}}}]}}]}}'
  # Row: object-name|collection-path|body-var (name == metadata.name)
  NSC_ROWS=(
    "nsc-dep|/apis/apps/v1/namespaces/$NS/deployments|NSC_DEP"
    "nsc-svc|/api/v1/namespaces/$NS/services|NSC_SVC"
    "nsc-cm|/api/v1/namespaces/$NS/configmaps|NSC_CM"
    "nsc-secret|/api/v1/namespaces/$NS/secrets|NSC_SEC"
    "nsc-ev|/api/v1/namespaces/$NS/events|NSC_EVT"
    "nsc-wf|/apis/argoproj.io/v1alpha1/namespaces/$NS/workflows|NSC_WF"
    "nsc-cwf|/apis/argoproj.io/v1alpha1/namespaces/$NS/cronworkflows|NSC_CWF"
    "nsc-ag|/apis/init-pro.io/v1/namespaces/$NS/agents|NSC_AGT"
    "nsc-lr|/apis/init-pro.io/v1/namespaces/$NS/luarouters|NSC_LR"
    "nsc-ing|/apis/networking.k8s.io/v1/namespaces/$NS/ingresses|NSC_ING"
  )
  NSC_POP_OK=1; NSC_POP_BAD=""; NSC_DEP_OK=0
  for row in "${NSC_ROWS[@]}"; do
    IFS='|' read -r NSC_NAME NSC_PATH NSC_VAR <<<"$row"
    NSC_C="$(nsc_post "$NSC_PATH" "${!NSC_VAR}")"
    NSC_G="$(nsc_code "$BASENSC$NSC_PATH/$NSC_NAME")"
    if [[ "$NSC_C" != "201" || "$NSC_G" != "200" ]]; then
      NSC_POP_BAD+=" $NSC_NAME(post=$NSC_C,get=$NSC_G)"
      NSC_POP_OK=0
    fi
    [[ "$NSC_NAME" == "nsc-dep" && "$NSC_C" == "201" ]] && NSC_DEP_OK=1
  done

  # Wait until BOTH replicas are Running on the agent's real containerd.
  NSC_RUNNING=0
  if [[ "$NSC_DEP_OK" -eq 1 ]]; then
    for _ in $(seq 1 120); do
      NSC_N="$(curl -s "$BASENSC/api/v1/namespaces/$NS/pods" 2>/dev/null | python3 -c 'import json,sys
try:
  items=json.load(sys.stdin).get("items",[])
  print(sum(1 for p in items if p.get("status",{}).get("phase")=="Running"))
except Exception: print(0)' 2>/dev/null || echo 0)"
      [[ "${NSC_N:-0}" -ge 2 ]] && { NSC_RUNNING=1; break; }
      sleep 0.5
    done
  fi

  # --- step 3: DELETE the namespace (soft delete stamps deletionTimestamp)
  NSC_DEL="$(nsc_code -X DELETE "$BASENSC/api/v1/namespaces/$NS")"
  # Best-effort observation of the soft-delete stamp; the drain may finalize
  # first, which is fine (fast drain, not an error) -- diagnostics only.
  NSC_SOFT="$(curl -s "$BASENSC/api/v1/namespaces/$NS" 2>/dev/null | python3 -c 'import json,sys
try:
  v=json.load(sys.stdin)
  print("stamped" if v.get("metadata",{}).get("deletionTimestamp") else "no-ts")
except Exception: print("gone")' 2>/dev/null || echo gone)"

  # --- G80: the drain reaches REAL containers ---------------------------
  # Bounded poll (~60s): zero golden-cascade pods left in the cluster-wide
  # pod list AND `crictl ps` on the agent data dir lists no containers.
  NSC_G80=0; NSC_LEFT="?" ; NSC_CIDS="?"
  if [[ "$NSC_NS_OK" -eq 1 && "$NSC_RUNNING" -eq 1 ]]; then
    for _ in $(seq 1 120); do
      NSC_LEFT="$(curl -s "$BASENSC/api/v1/pods" 2>/dev/null | python3 -c 'import json,sys
try:
  items=json.load(sys.stdin).get("items",[])
  print(sum(1 for p in items if p.get("metadata",{}).get("namespace")=="'"$NS"'"))
except Exception: print(99)' 2>/dev/null || echo 99)"
      NSC_CIDS="$(INIT_PRO_DATA_DIR="$NSCTMP/ag" "$BIN" crictl ps -q 2>/dev/null | grep -c . || true)"
      if [[ "${NSC_LEFT:-99}" -eq 0 && "${NSC_CIDS:-1}" -eq 0 ]]; then NSC_G80=1; break; fi
      sleep 0.5
    done
  fi
  if [[ "$NSC_G80" -eq 1 ]]; then
    ok "G80  ns DELETE drains the 2 RUNNING pods: REST list empty + crictl ps empty  (T3.1b drain, Q20)"
    PASS=$((PASS+1))
  else
    echo "FAIL G80 drain did not reach real containers (ns=$NSC_NS_CODE/$NSC_FIN dep=$NSC_DEP_OK running=$NSC_RUNNING del=$NSC_DEL soft=$NSC_SOFT left=$NSC_LEFT cids=$NSC_CIDS; srv: $NSCSRV ag: $NSCAG)"
    FAIL=$((FAIL+1))
  fi

  # --- G81: DRAIN_KINDS closure -- every namespaced kind drains ----------
  # The 6 leaked kinds first (the regression surface), then the whole
  # served namespaced footprint (12 pre-existing + 6 fixed = 18 lists).
  NSC_SIX=(events workflows cronworkflows agents luarouters ingresses)
  NSC_PATHS=(
    "/api/v1/namespaces/$NS/events"
    "/apis/argoproj.io/v1alpha1/namespaces/$NS/workflows"
    "/apis/argoproj.io/v1alpha1/namespaces/$NS/cronworkflows"
    "/apis/init-pro.io/v1/namespaces/$NS/agents"
    "/apis/init-pro.io/v1/namespaces/$NS/luarouters"
    "/apis/networking.k8s.io/v1/namespaces/$NS/ingresses"
    "/api/v1/namespaces/$NS/pods"
    "/api/v1/namespaces/$NS/services"
    "/api/v1/namespaces/$NS/endpoints"
    "/api/v1/namespaces/$NS/configmaps"
    "/api/v1/namespaces/$NS/secrets"
    "/api/v1/namespaces/$NS/persistentvolumeclaims"
    "/apis/apps/v1/namespaces/$NS/deployments"
    "/apis/apps/v1/namespaces/$NS/replicasets"
    "/apis/apps/v1/namespaces/$NS/statefulsets"
    "/apis/apps/v1/namespaces/$NS/daemonsets"
    "/apis/apps/v1/namespaces/$NS/controllerrevisions"
    "/apis/coordination.k8s.io/v1/namespaces/$NS/leases"
  )
  NSC_G81=0; NSC_BAD=""
  if [[ "$NSC_NS_OK" -eq 1 && "$NSC_POP_OK" -eq 1 ]]; then
    for _ in $(seq 1 120); do
      NSC_BAD=""
      for p in "${NSC_PATHS[@]}"; do
        [[ "$(nsc_items "$p")" == "0" ]] || NSC_BAD+=" ${p##*/}"
      done
      [[ -z "$NSC_BAD" ]] && { NSC_G81=1; break; }
      sleep 0.5
    done
  fi
  if [[ "$NSC_G81" -eq 1 ]]; then
    ok "G81  all 18 namespaced lists empty incl. the 6 leaked kinds (${NSC_SIX[*]})  (D1 fix, Q20)"
    PASS=$((PASS+1))
  else
    echo "FAIL G81 drain left objects behind (pop_bad=${NSC_POP_BAD:-none} non_empty=${NSC_BAD:-unknown})"
    FAIL=$((FAIL+1))
  fi

  # --- G82: terminal delete + a quiet controller log ---------------------
  NSC_G82=0
  for _ in $(seq 1 120); do
    [[ "$(nsc_code "$BASENSC/api/v1/namespaces/$NS")" == "404" ]] && { NSC_G82=1; break; }
    sleep 0.5
  done
  NSC_LOGERR=""
  if [[ "$NSC_G82" -eq 1 ]]; then
    # No repeated drain/finalize errors in the server log tail: the
    # controller must converge, not spin on a namespace it cannot drain.
    NSC_LOGERR="$(tail -n 400 "$NSCSRV" 2>/dev/null | grep -wE 'ERROR|WARN' \
      | grep -iE 'drain|finaliz' || true)"
    NSC_NSERR="$(tail -n 400 "$NSCSRV" 2>/dev/null | grep -wE 'ERROR|WARN' \
      | grep -F "$NS" || true)"
    [[ -z "$NSC_LOGERR" && -z "$NSC_NSERR" ]] || NSC_G82=0
  fi
  if [[ "$NSC_G82" -eq 1 ]]; then
    ok "G82  namespace terminal-deletes (GET 404) + no drain/finalize errors in log  (T3.1b, Q19/Q20)"
    PASS=$((PASS+1))
  else
    echo "FAIL G82 namespace did not finalize cleanly (ns404=$NSC_G82 log='${NSC_LOGERR:-}' nserr='${NSC_NSERR:-}'; srv: $NSCSRV)"
    FAIL=$((FAIL+1))
  fi

  # --- hygiene (G30 tail): kill the pair, then zero-shim assertion -------
  # Bounded wait so the kubelet/containerd teardown finishes before the
  # rm -rf (the pair must be drained, not abandoned mid-teardown).
  for _ in $(seq 1 40); do
    pgrep -f "$NSCTMP/.*containerd-shim" >/dev/null 2>&1 || break
    sleep 0.25
  done
  kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; SRV2=""
  kill -TERM "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; AGENT_PID=""
  assert_no_run_shims "$NSCTMP" || FAIL=$((FAIL+1))
  rm -rf "$NSCTMP" "$NSCSRV" "$NSCAG" 2>/dev/null || true
fi
