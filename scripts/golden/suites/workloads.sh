# Real-workload behavior suite (B1 STS + B2 DS + B4 CronWorkflow tick): real
# pods over a self-contained server+agent pair (G25/G29 pattern).
# Sourced by golden-conformance.sh; runs in the main script's shell
# (helpers: check_body/check_status/check_method/ok, $BASE, $GOLDEN, $BIN).
# Owned by plan item: B1+B2+B4. Check-ID range reserved: G60-G66. Hard cap 400 lines.
#
#   G60  StatefulSet ordinals run as REAL containerd sandboxes on the agent
#        node; per-ordinal PVCs exist and stay Pending (Q22: never Bound).
#   G61  OrderedReady: ordinal 1 must not exist while ordinal 0 is not Ready.
#   G62  DaemonSet: exactly one real pod, pinned to the single ready Node.
#   G63  Node deletion: the DS pod bound to the gone Node is deleted, the Node
#        does not resurrect, no orphan DS pod stays.
#   G64  CronWorkflow tick fires a child Workflow `<cwf>-<unix>` with the
#        upstream child label + CronWorkflow ownerReference.
#   G65  the child Workflow runs to Succeeded on the agent (airgap wfstep).
#   G66  no stampede: at most one child per due cron tick (minute boundary).
#
# ONE pair serves all of B1/B2/B4 (three G25-style pairs would triple the
# wall clock for no extra coverage). Order inside the block: STS -> DS
# placement -> CronWorkflow -> node deletion LAST: the kubelet registers its
# Node exactly once (crates/kubelet/src/node.rs `ensure_node` runs before the
# heartbeat loop, which only GETs) so a deleted Node never comes back and
# nothing created after G63 could run. Suite-local vars only (WL_*); SRV2 and
# AGENT_PID are handed back cleared, exactly like G25/G29. Vendor+cc
# detection and the SKIP-not-fail gates mirror G25 verbatim (Q25).

WL_VENDOR_BIN=""
if [[ -n "${INIT_PRO_VENDOR_BIN:-}" && -f "$INIT_PRO_VENDOR_BIN/containerd" ]]; then
  WL_VENDOR_BIN="$INIT_PRO_VENDOR_BIN"
else
  for c in "$(dirname "$BIN")/../../vendor/bin" "$(dirname "$BIN")/../vendor/bin" "$PWD/vendor/bin" "$ROOT/vendor/bin"; do
    if [[ -f "$c/containerd" ]]; then WL_VENDOR_BIN="$c"; break; fi
  done
fi

if [[ -z "$WL_VENDOR_BIN" ]]; then
  echo "SKIP G60-G66 real-workload behavior (no vendored containerd; build with INIT_PRO_VENDOR=1)"
elif ! command -v cc >/dev/null 2>&1; then
  echo "SKIP G60-G66 real-workload behavior (no cc for the airgap images, Q27)"
else
  WLTMP="$(mktemp -d)"; WLSRV="$(mktemp)"; WLAG="$(mktemp)"
  WL_PORT="$(pick_port)"
  "$BIN" server --data-dir "$WLTMP/srv" --bind-address 127.0.0.1 --https-listen-port "$WL_PORT" >"$WLSRV" 2>&1 &
  SRV2=$!
  for _ in $(seq 1 60); do grep -q "discovery listening" "$WLSRV" && break; sleep 0.1; done
  WLBASE="http://127.0.0.1:${WL_PORT}"
  WL_NODE="wl-node"
  WL_PAUSE_REF="init-pro.local/pause:0.1"
  WL_STEP_REF="init-pro.local/wfstep:0.1"
  "$SCRIPT_DIR/build-pause-image.sh" "$WLTMP/pause.tar" "$WL_PAUSE_REF" >/dev/null 2>&1 || true
  "$SCRIPT_DIR/build-workflow-image.sh" "$WLTMP/wfstep.tar" "$WL_STEP_REF" >/dev/null 2>&1 || true
  INIT_PRO_SANDBOX_IMAGE="$WL_PAUSE_REF" "$BIN" agent --data-dir "$WLTMP/ag" --token dev --disable-agent-exec \
      --server "$WLBASE" --node-name "$WL_NODE" >"$WLAG" 2>&1 &
  AGENT_PID=$!
  for _ in $(seq 1 300); do grep -q "containerd healthy" "$WLAG" && break; sleep 0.1; done
  INIT_PRO_DATA_DIR="$WLTMP/ag" "$BIN" ctr -n k8s.io images import "$WLTMP/pause.tar" >/dev/null 2>&1 || true
  INIT_PRO_DATA_DIR="$WLTMP/ag" "$BIN" ctr -n k8s.io images import "$WLTMP/wfstep.tar" >/dev/null 2>&1 || true
  WL_PODS="$WLBASE/api/v1/namespaces/default/pods"

  # Suite-local readers. wl_pods prints "<name> <phase> <Ready|-> <node>" per
  # pod whose name starts with $1; wl_sandboxes counts READY CRI sandboxes by
  # the same prefix (the real-containerd leg of G60/G62).
  wl_pods() {
    curl -s --max-time 10 "$WL_PODS" | python3 -c '
import json,sys
prefix=sys.argv[1]
try: items=json.load(sys.stdin).get("items",[])
except Exception: items=[]
for p in items:
    m=p.get("metadata",{})
    n=m.get("name","")
    if not n.startswith(prefix): continue
    s=p.get("status") or {}
    c={x.get("type"):x.get("status") for x in s.get("conditions",[])}
    print(n, s.get("phase",""), c.get("Ready","-"), p.get("spec",{}).get("nodeName","-"))
' "$1" 2>/dev/null || true
  }
  wl_sandboxes() {
    INIT_PRO_DATA_DIR="$WLTMP/ag" "$BIN" crictl pods -o json 2>/dev/null | python3 -c '
import json,sys
prefix=sys.argv[1]
try: items=json.load(sys.stdin).get("items",[])
except Exception: items=[]
print(sum(1 for i in items
          if i.get("metadata",{}).get("name","").startswith(prefix)
          and i.get("state") == "SANDBOX_READY"))' "$1" 2>/dev/null || echo 0
  }

  # ---- B1 (G60): StatefulSet, 2 replicas, per-ordinal claims ----
  cat > "$WLTMP/sts.json" <<EOF
{"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"g60-sts","namespace":"default"},
 "spec":{"replicas":2,"serviceName":"g60-sts","selector":{"matchLabels":{"app":"g60-sts"}},
  "template":{"metadata":{"labels":{"app":"g60-sts"}},
   "spec":{"containers":[{"name":"pause","image":"$WL_PAUSE_REF"}]}},
  "volumeClaimTemplates":[{"metadata":{"name":"data"},
   "spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Mi"}}}}]}}
EOF
  WL_STS_POST="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary "@$WLTMP/sts.json" "$WLBASE/apis/apps/v1/namespaces/default/statefulsets")"
  WL_STS_READY=0
  for _ in $(seq 1 180); do
    ST="$(wl_pods g60-sts-)"
    if grep -q '^g60-sts-0 Running True ' <<<"$ST" && grep -q '^g60-sts-1 Running True ' <<<"$ST"; then
      WL_STS_READY=1; break
    fi
    sleep 0.5
  done
  WL_ON_NODE="$(grep -c "wl-node$" <<<"$(wl_pods g60-sts-)" || true)"
  WL_SBX="$(wl_sandboxes g60-sts-)"
  WL_PVC="$(curl -s "$WLBASE/api/v1/namespaces/default/persistentvolumeclaims" | python3 -c '
import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: items=[]
ph={i["metadata"]["name"]:(i.get("status") or {}).get("phase") for i in items
    if i["metadata"]["name"].startswith("data-g60-sts-")}
need={"data-g60-sts-0","data-g60-sts-1"}
good = need <= set(ph) and all(v in (None,"Pending") for v in ph.values())
print("PVC-OK" if good else "PVC-BAD %s" % sorted(ph.items()))' 2>/dev/null || true)"
  if [[ "$WL_STS_POST" == "201" && "$WL_STS_READY" -eq 1 && "$WL_ON_NODE" -eq 2 \
        && "$WL_SBX" -ge 2 && "$WL_PVC" == "PVC-OK" ]]; then
    ok "G60  statefulset ordinals Running+Ready as real sandboxes; per-ordinal PVCs stay Pending  (T3.1b/Q22, real stack)"
    PASS=$((PASS+1))
  else
    echo "FAIL G60  statefulset on the real stack (post=$WL_STS_POST ready=$WL_STS_READY on-node=$WL_ON_NODE sandboxes=$WL_SBX $WL_PVC; server: $WLSRV agent: $WLAG)"
    wl_pods g60-sts- >&2 || true
    FAIL=$((FAIL+1))
  fi

  # ---- B1 (G61): OrderedReady must not create ordinal 1 while ordinal 0 is
  # not Ready. Probe: wfstep --fail exits 1 under the default restartPolicy
  # Always, so g61-sts-0 restarts forever and stays Pending/Ready=False
  # (kubelet/src/status.rs) — upstream OrderedReady then never creates
  # ordinal 1. controllers/statefulset.rs sets `prev_ready = true` right
  # after a fresh create, so watch what the code really does and report it.
  cat > "$WLTMP/sts61.json" <<EOF
{"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"g61-sts","namespace":"default"},
 "spec":{"replicas":2,"serviceName":"g61-sts","selector":{"matchLabels":{"app":"g61-sts"}},
  "template":{"metadata":{"labels":{"app":"g61-sts"}},
   "spec":{"containers":[{"name":"c","image":"$WL_STEP_REF","command":["wfstep"],"args":["--fail","g61"]}]}}}}
EOF
  WL61_POST="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary "@$WLTMP/sts61.json" "$WLBASE/apis/apps/v1/namespaces/default/statefulsets")"
  WL61_ORD0=0; WL61_BREACH=0
  for _ in $(seq 1 40); do
    ST61="$(wl_pods g61-sts-)"
    grep -q '^g61-sts-0 ' <<<"$ST61" && WL61_ORD0=1
    if grep -q '^g61-sts-1 ' <<<"$ST61"; then WL61_BREACH=1; break; fi
    sleep 0.5
  done
  WL61_NOTREADY="$(curl -s "$WLBASE/apis/apps/v1/namespaces/default/statefulsets/g61-sts" \
    | python3 -c 'import json,sys
try: print((json.load(sys.stdin).get("status") or {}).get("readyReplicas",0))
except Exception: print("?")' 2>/dev/null || true)"
  WL61_STATE="$(wl_pods g61-sts-)"
  curl -s -o /dev/null -X DELETE "$WLBASE/apis/apps/v1/namespaces/default/statefulsets/g61-sts" || true
  for _ in $(seq 1 60); do
    [[ -z "$(wl_pods g61-sts-)" ]] && break
    sleep 0.5
  done
  if [[ "$WL61_POST" == "201" && "$WL61_ORD0" -eq 1 && "$WL61_BREACH" -eq 0 ]]; then
    ok "G61  orderedready: ordinal 1 withheld while ordinal 0 never went Ready  (T3.1b, real stack)"
    PASS=$((PASS+1))
  else
    if [[ "$WL61_BREACH" -eq 1 ]]; then
      echo "FAIL G61  orderedready breached: g61-sts-1 exists while g61-sts-0 is not Ready (post=$WL61_POST ord0=$WL61_ORD0 sts.readyReplicas=$WL61_NOTREADY)"
    else
      echo "FAIL G61  orderedready probe inconclusive: ordinal 0 never observed (post=$WL61_POST ord0=$WL61_ORD0 breach=$WL61_BREACH sts.readyReplicas=$WL61_NOTREADY)"
    fi
    echo "     observed pods: $WL61_STATE"
    echo "     evidence: controllers/statefulset.rs create loop sets prev_ready=true for a fresh pod, so ordinals 0+1 are created in ONE reconcile (module doc + plan/03 claim OrderedReady gating)."
    FAIL=$((FAIL+1))
  fi

  # ---- B2 (G62): DaemonSet, one real pod pinned to the single ready Node.
  # No nodeSelector: an absent/empty constraint matches every node
  # (controllers/daemonset.rs `node_matches`), so the DS must place exactly
  # one pod on wl-node (the pair has exactly one Node) as a REAL sandbox.
  cat > "$WLTMP/ds.json" <<EOF
{"apiVersion":"apps/v1","kind":"DaemonSet","metadata":{"name":"g62-ds","namespace":"default"},
 "spec":{"selector":{"matchLabels":{"app":"g62-ds"}},
  "template":{"metadata":{"labels":{"app":"g62-ds"}},
   "spec":{"containers":[{"name":"pause","image":"$WL_PAUSE_REF"}]}}}}
EOF
  WL_DS_POST="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary "@$WLTMP/ds.json" "$WLBASE/apis/apps/v1/namespaces/default/daemonsets")"
  WL_NODES="$(curl -s "$WLBASE/api/v1/nodes" | python3 -c '
import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: items=[]
print(" ".join(sorted(i["metadata"]["name"] for i in items)))' 2>/dev/null || true)"
  WL_DS_OK=0
  for _ in $(seq 1 90); do
    SD="$(wl_pods g62-ds-)"
    if [[ "$(grep -c "^g62-ds-.* $WL_NODE$" <<<"$SD" || true)" -eq 1 ]] \
       && [[ "$(wc -l <<<"$SD")" -eq 1 ]] \
       && grep -q "^g62-ds-.* Running True $WL_NODE$" <<<"$SD" \
       && [[ "$(wl_sandboxes g62-ds-)" -eq 1 ]] \
       && curl -s "$WLBASE/apis/apps/v1/namespaces/default/daemonsets/g62-ds" \
            | grep -q '"desiredNumberScheduled":1'; then
      WL_DS_OK=1; break
    fi
    sleep 0.5
  done
  if [[ "$WL_DS_POST" == "201" && "$WL_NODES" == "$WL_NODE" && "$WL_DS_OK" -eq 1 ]]; then
    ok "G62  daemonset: exactly 1 real pod pinned to the only ready node ($WL_NODE)  (T3.1b, real stack)"
    PASS=$((PASS+1))
  else
    echo "FAIL G62  daemonset placement (post=$WL_DS_POST nodes='$WL_NODES' ok=$WL_DS_OK pods: $(wl_pods g62-ds-))"
    FAIL=$((FAIL+1))
  fi

  # ---- B4 (G64-G66): CronWorkflow real tick. `* * * * *` avoids timezone
  # edges (the engine is UTC/naive, cronworkflow.rs + cron_expr.rs) and the
  # first tick fires eagerly (no anchor on disk yet). Forbid exercises the
  # policy field; the child inherits spec.workflowSpec verbatim.
  python3 - "$WLTMP/cwf.json" "$WL_STEP_REF" <<'PY'
import json,sys
spec={"schedule":"* * * * *","concurrencyPolicy":"Forbid",
 "workflowSpec":{"entrypoint":"main","templates":[{"name":"main","container":{"name":"main",
 "image":sys.argv[2],"command":["wfstep"],"args":["g64"]}}]}}
json.dump({"apiVersion":"argoproj.io/v1alpha1","kind":"CronWorkflow",
 "metadata":{"name":"g64-cwf","namespace":"default"},"spec":spec},open(sys.argv[1],"w"))
PY
  WL_CWF_POST="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary "@$WLTMP/cwf.json" "$WLBASE/apis/argoproj.io/v1alpha1/namespaces/default/cronworkflows")"
  WL_WFS="$WLBASE/apis/argoproj.io/v1alpha1/namespaces/default/workflows"
  # wl_children prints "<name> <phase>" for every Workflow in default/.
  wl_children() {
    curl -s "$WL_WFS" | python3 -c '
import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: items=[]
for w in items:
    print(w.get("metadata",{}).get("name",""), w.get("status",{}).get("phase",""))' 2>/dev/null || true
  }
  # G64: the tick fires `<cwf>-<unix-seconds>` with the upstream child label
  # workflows.argoproj.io/cron-workflow + a CronWorkflow ownerReference, and
  # only a real fire advances status.lastScheduledTime (cronworkflow.rs fire).
  WL_CHILD=""
  for _ in $(seq 1 60); do
    WL_CHILD="$(wl_children | awk '$1 ~ /^g64-cwf-[0-9]+$/ {print $1; exit}')"
    [[ -n "$WL_CHILD" ]] && break
    sleep 0.5
  done
  WL64_META=""
  if [[ -n "$WL_CHILD" ]]; then
    WL64_META="$(curl -s "$WL_WFS/$WL_CHILD" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: d={}
m=d.get("metadata",{})
lbl=(m.get("labels") or {}).get("workflows.argoproj.io/cron-workflow","")
own=[o.get("kind") for o in m.get("ownerReferences",[])]
print("CHILD-OK" if lbl=="g64-cwf" and "CronWorkflow" in own else "CHILD-BAD")' 2>/dev/null || true)"
  fi
  WL64_ANCHOR="$(curl -s "$WLBASE/apis/argoproj.io/v1alpha1/namespaces/default/cronworkflows/g64-cwf" \
    | grep -c lastScheduledTime || true)"
  if [[ "$WL_CWF_POST" == "201" && -n "$WL_CHILD" && "$WL64_META" == "CHILD-OK" && "$WL64_ANCHOR" -ge 1 ]]; then
    ok "G64  cronworkflow tick fired child $WL_CHILD (label + ownerRef + lastScheduledTime)  (T7.3 Scope C, real stack)"
    PASS=$((PASS+1))
  else
    echo "FAIL G64  cronworkflow child (post=$WL_CWF_POST child='$WL_CHILD' meta=$WL64_META anchor=$WL64_ANCHOR)"
    FAIL=$((FAIL+1))
  fi

  # G65: the child executes to Succeeded on the agent (airgap wfstep step,
  # restartPolicy Never -> the 0 exit is the verdict).
  WL_PHASE=""
  for _ in $(seq 1 180); do
    WL_PHASE="$(wl_children | awk -v c="$WL_CHILD" '$1==c {print $2; exit}')"
    [[ "$WL_PHASE" == "Succeeded" || "$WL_PHASE" == "Failed" || "$WL_PHASE" == "Error" ]] && break
    sleep 0.5
  done
  if [[ "$WL_PHASE" == "Succeeded" ]]; then
    ok "G65  cronworkflow child Workflow Succeeded on the agent/containerd pair  (T7.3 Scope C, real stack)"
    PASS=$((PASS+1))
  else
    echo "FAIL G65  child workflow phase=$WL_PHASE (child='$WL_CHILD'; server: $WLSRV agent: $WLAG)"
    FAIL=$((FAIL+1))
  fi

  # G66: no stampede — with `* * * * *` the only further due points are the
  # minute boundaries after the eager fire, so the child count may never
  # exceed 1 + crossed boundaries (one child per tick, lastScheduledTime
  # anchor). A child-per-reconcile bug blows past the bound immediately.
  WL_STAMP=1
  for _ in $(seq 1 24); do
    WL_STAMP_V="$(curl -s "$WL_WFS" | python3 -c '
import json,sys,time
try: items=json.load(sys.stdin).get("items",[])
except Exception: items=[]
fires=[int(w["metadata"]["name"].rsplit("-",1)[1]) for w in items
       if w.get("metadata",{}).get("name","").startswith("g64-cwf-")
       and w["metadata"]["name"].rsplit("-",1)[1].isdigit()]
now=int(time.time())
bound=(1 + sum(1 for b in range(min(fires)+1,now+1) if b%60==0)) if fires else 0
print("STAMP-OK" if len(fires)>=1 and len(fires)<=bound
      else "STAMP-BAD count=%d bound=%d" % (len(fires), bound))' 2>/dev/null || true)"
    [[ "$WL_STAMP_V" == "STAMP-OK" ]] || { WL_STAMP=0; break; }
    sleep 1
  done
  # The anchor must be one of the real fire seconds (it advances only on a
  # fire); give the write-if-changed status a short bounded settle.
  WL_KIDS="$(wl_children | awk '$1 ~ /^g64-cwf-[0-9]+$/ {print $1}')"
  WL_ANCHOR=0
  for _ in $(seq 1 12); do
    WL_ANCHOR="$(curl -s "$WLBASE/apis/argoproj.io/v1alpha1/namespaces/default/cronworkflows/g64-cwf" | python3 -c '
import json,sys,calendar,time
names=sys.argv[1:]
try: d=json.load(sys.stdin)
except Exception: d={}
ts=(d.get("status") or {}).get("lastScheduledTime") or ""
if not ts or not names:
    print(0); raise SystemExit
try:
    t=calendar.timegm(time.strptime(ts,"%Y-%m-%dT%H:%M:%SZ"))
    fires={int(n.rsplit("-",1)[1]) for n in names if n.rsplit("-",1)[1].isdigit()}
    print(1 if t in fires else 0)
except Exception:
    print(0)' $WL_KIDS 2>/dev/null || true)"
    [[ "$WL_ANCHOR" == "1" ]] && break
    sleep 0.5
  done
  if [[ "$WL_STAMP" -eq 1 && "$WL_ANCHOR" == "1" ]]; then
    ok "G66  no stampede: 1 child per due tick over the window + anchored lastScheduledTime  (T7.3 Scope C, real stack)"
    PASS=$((PASS+1))
  else
    echo "FAIL G66  cronworkflow tick discipline ($WL_STAMP_V anchor-matches-fire=$WL_ANCHOR children: $(wl_children | tr '\n' ' '))"
    FAIL=$((FAIL+1))
  fi
  # Owner delete first: GC cascades the children, the kubelet tears their
  # sandboxes down before the pair stops.
  curl -s -o /dev/null -X DELETE "$WLBASE/apis/argoproj.io/v1alpha1/namespaces/default/cronworkflows/g64-cwf" || true
  for _ in $(seq 1 60); do
    [[ -z "$(wl_children | awk '$1 ~ /^g64-cwf-/')" ]] && break
    sleep 0.5
  done

  # ---- B2 (G63): node-deletion following — LAST (see the block comment):
  # delete the Node object and require the controller + kubelet truth: the DS
  # pod bound to the gone Node is deleted (daemonset.rs placement_victims:
  # pods on non-matching nodes are victims — this IS the node-lifecycle
  # reaction), the Node does NOT resurrect (kubelet/src/node.rs registers
  # once; the heartbeat only GETs), and no orphan DS pod appears.
  WL_NODE_DEL="$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$WLBASE/api/v1/nodes/$WL_NODE")"
  WL63_GONE=0; WL63_ORPHAN=""; WL63_CODE=""; DS63="?"
  for _ in $(seq 1 40); do
    WL63_CODE="$(curl -s -o /dev/null -w '%{http_code}' "$WLBASE/api/v1/nodes/$WL_NODE" || true)"
    SD63="$(wl_pods g62-ds-)"
    DS63="$(curl -s "$WLBASE/apis/apps/v1/namespaces/default/daemonsets/g62-ds" | python3 -c '
import json,sys
try: print((json.load(sys.stdin).get("status") or {}).get("desiredNumberScheduled","?"))
except Exception: print("?")' 2>/dev/null || true)"
    if [[ "$WL63_CODE" == "404" && -z "$SD63" && "$DS63" == "0" ]]; then
      WL63_GONE=1; break
    fi
    [[ -n "$SD63" ]] && WL63_ORPHAN="$SD63"
    sleep 0.5
  done
  if [[ "$WL_NODE_DEL" == "200" && "$WL63_GONE" -eq 1 && "$WL_DS_OK" -eq 1 ]]; then
    ok "G63  node delete: DS pod on the gone node deleted, node stays gone, no orphan  (T3.1b, real stack)"
    PASS=$((PASS+1))
  elif [[ "$WL_DS_OK" -ne 1 ]]; then
    echo "FAIL G63  node-deletion following skipped: the G62 leg never observed the DS pod on $WL_NODE, so this verdict would be vacuous (del=$WL_NODE_DEL node404=$WL63_CODE orphan-desired=$DS63)"
    FAIL=$((FAIL+1))
  else
    echo "FAIL G63  node-deletion following (del=$WL_NODE_DEL node404=$WL63_CODE orphan-desired=$DS63 pods='$WL63_ORPHAN')"
    FAIL=$((FAIL+1))
  fi

  # Teardown mirroring G25/G29: owners already deleted (g61/g64 above; g60
  # here) so GC cascades the pods and the kubelet tears every sandbox down
  # BEFORE the agent stops containerd — no mounts pinned under the data-dir.
  curl -s -o /dev/null -X DELETE "$WLBASE/apis/apps/v1/namespaces/default/statefulsets/g60-sts" || true
  for _ in $(seq 1 120); do
    CNT="$(wl_sandboxes "")"
    [[ "$CNT" == "0" ]] && break
    sleep 0.5
  done
  kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; SRV2=""
  kill -TERM "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; AGENT_PID=""
  assert_no_run_shims "$WLTMP" || FAIL=$((FAIL+1))
  rm -rf "$WLTMP" "$WLSRV" "$WLAG" 2>/dev/null || true
fi
