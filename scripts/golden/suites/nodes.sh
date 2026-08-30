# Node-layer suite (G24-G26): T4.1 agent-supervised containerd + crictl
# over CRI; T4.2 kubelet drives real pods end-to-end (G25 lifecycle,
# G26 probes/volumes/hostname). Sourced by golden-conformance.sh.
# --- T4.1: agent-supervised containerd + crictl over CRI (G24) ---
# The agent stages the vendored runtime tree (Q24) and supervises containerd
# through the multicall seam (Q25); `crictl` must reach the CRI socket purely
# via INIT_PRO_DATA_DIR (k3s agent parity). The image-pull smoke is
# best-effort (Q25): without a reachable registry it SKIPs and does NOT
# touch FAIL — but the supervision + CRI checks themselves hard-fail WHEN a
# vendor bundle is present. CI builds with INIT_PRO_VENDOR=0 ship no
# vendor/bin tree (ci.yml lint-test), so the whole gate SKIPs then — aligned
# with the Q25 SKIP-not-fail policy and the runtime integration test: a
# missing vendor bundle is a build-configuration state, not a conformance
# break. Vendor detection mirrors runtime::stage::vendor_bin_root():
# INIT_PRO_VENDOR_BIN override, else exe-relative, else cwd-relative.
G24_VENDOR_BIN=""
if [[ -n "${INIT_PRO_VENDOR_BIN:-}" && -f "$INIT_PRO_VENDOR_BIN/containerd" ]]; then
  G24_VENDOR_BIN="$INIT_PRO_VENDOR_BIN"
else
  for c in "$(dirname "$BIN")/../../vendor/bin" "$(dirname "$BIN")/../vendor/bin" "$PWD/vendor/bin" "$ROOT/vendor/bin"; do
    if [[ -f "$c/containerd" ]]; then G24_VENDOR_BIN="$c"; break; fi
  done
fi
if [[ -z "$G24_VENDOR_BIN" ]]; then
  echo "SKIP G24 agent runtime (no vendored containerd; build with INIT_PRO_VENDOR=1)"
else
  G24=0
  G24TMP="$(mktemp -d)"; LOG24="$(mktemp)"
  "$BIN" agent --data-dir "$G24TMP/g24-dd" --token dev --server https://127.0.0.1:6443 >"$LOG24" 2>&1 &
  AGENT_PID=$!
  for _ in $(seq 1 200); do grep -q "containerd healthy" "$LOG24" && break; sleep 0.1; done
  G24_V="$(INIT_PRO_DATA_DIR="$G24TMP/g24-dd" "$BIN" crictl version 2>&1 || true)"
  if INIT_PRO_DATA_DIR="$G24TMP/g24-dd" "$BIN" crictl ps >/dev/null 2>&1; then G24_PS=1; else G24_PS=0; fi
  if grep -q "containerd healthy" "$LOG24" && grep -q "RuntimeName" <<<"$G24_V" \
     && grep -q "containerd" <<<"$G24_V" && [[ "$G24_PS" -eq 1 ]]; then
    ok "G24  agent supervises containerd; crictl version/ps over CRI  (runtime, T4.1)"
    PASS=$((PASS+1)); G24=1
  else
    echo "FAIL G24  agent runtime (check $LOG24; version: $G24_V ps=$G24_PS)"
    FAIL=$((FAIL+1))
  fi
  if [[ "$G24" -eq 1 ]]; then
    if timeout 60 env INIT_PRO_DATA_DIR="$G24TMP/g24-dd" "$BIN" crictl pull registry.k8s.io/pause:3.10 >/dev/null 2>&1; then
      ok "G24  crictl pull pause sandbox image over CRI  (runtime, T4.1)"
      PASS=$((PASS+1))
    else
      echo "SKIP g24 sandbox smoke (registry unreachable)"
    fi
  fi
  kill -TERM "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; AGENT_PID=""
  purge_run_shims "$G24TMP"
  rm -rf "$G24TMP" "$LOG24" 2>/dev/null || true
fi

# --- T4.2: kubelet runs a real pod end-to-end (G25) ---
# Same vendor gate as G24 (SKIP without a vendored containerd) plus a `cc`
# gate: the workload image is the Q27 airgap pause — assembled locally from
# a static binary (scripts/build-pause-image.sh), imported through the
# staged `ctr`, never pulled from a registry. Flow: Deployment -> RS -> pod
# -> scheduler binds g25-node -> kubelet drives CRI -> Running+Ready;
# kill the container -> kubelet restarts it; delete the Deployment -> GC +
# kubelet teardown empty every sandbox.
if [[ -z "$G24_VENDOR_BIN" ]]; then
  echo "SKIP G25 kubelet end-to-end (no vendored containerd)"
elif ! command -v cc >/dev/null 2>&1; then
  echo "SKIP G25 kubelet end-to-end (no cc for the airgap pause image, Q27)"
else
  G25TMP="$(mktemp -d)"; G25SRV="$(mktemp)"; G25AG="$(mktemp)"
  G25_PORT="$(pick_port)"
  "$BIN" server --data-dir "$G25TMP/srv" --bind-address 127.0.0.1 --https-listen-port "$G25_PORT" >"$G25SRV" 2>&1 &
  SRV2=$!
  for _ in $(seq 1 60); do grep -q "discovery listening" "$G25SRV" && break; sleep 0.1; done
  BASE25="http://127.0.0.1:${G25_PORT}"
  PAUSE_REF="init-pro.local/pause:0.1"
  G25_IMG="$G25TMP/pause.tar"
  "$SCRIPT_DIR/build-pause-image.sh" "$G25_IMG" "$PAUSE_REF" >/dev/null 2>&1 || true
  INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" "$BIN" agent --data-dir "$G25TMP/ag" --token dev \
      --server "$BASE25" --node-name g25-node >"$G25AG" 2>&1 &
  AGENT_PID=$!
  for _ in $(seq 1 300); do grep -q "containerd healthy" "$G25AG" && break; sleep 0.1; done
  INIT_PRO_DATA_DIR="$G25TMP/ag" "$BIN" ctr -n k8s.io images import "$G25_IMG" >/dev/null 2>&1 || true
  curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
    -d '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"g25-dep","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"g25"}},"template":{"metadata":{"labels":{"app":"g25"}},"spec":{"containers":[{"name":"pause","image":"'"$PAUSE_REF"'"}]}}}}' \
    "$BASE25/apis/apps/v1/namespaces/default/deployments"
  G25A=0
  for _ in $(seq 1 240); do
    S="$(curl -s "$BASE25/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for p in items:
  st=p.get("status",{})
  conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
  if st.get("phase")=="Running" and conds.get("Ready")=="True": print("READY"); break' 2>/dev/null || true)"
    [[ "$S" == "READY" ]] && { G25A=1; break; }
    sleep 0.5
  done
  if [[ "$G25A" -eq 1 ]]; then
    ok "G25  deployment pod Running+Ready on the agent node  (kubelet, T4.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G25a pod never Running+Ready (server: $G25SRV agent: $G25AG)"
    FAIL=$((FAIL+1))
  fi
  G25B=0
  if [[ "$G25A" -eq 1 ]]; then
    CID="$(INIT_PRO_DATA_DIR="$G25TMP/ag" "$BIN" crictl ps -q 2>/dev/null | head -1)"
    if [[ -n "$CID" ]]; then
      INIT_PRO_DATA_DIR="$G25TMP/ag" "$BIN" crictl stop --timeout 5 "$CID" >/dev/null 2>&1 || true
      for _ in $(seq 1 120); do
        NID="$(INIT_PRO_DATA_DIR="$G25TMP/ag" "$BIN" crictl ps -q 2>/dev/null | head -1)"
        if [[ -n "$NID" && "$NID" != "$CID" ]]; then G25B=1; break; fi
        sleep 0.5
      done
    fi
  fi
  if [[ "$G25B" -eq 1 ]]; then
    ok "G25  killed container restarted by the kubelet  (kubelet, T4.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G25b container restart (agent log: $G25AG)"
    FAIL=$((FAIL+1))
  fi
  G25C=0
  curl -s -o /dev/null -X DELETE "$BASE25/apis/apps/v1/namespaces/default/deployments/g25-dep"
  for _ in $(seq 1 120); do
    CNT="$(INIT_PRO_DATA_DIR="$G25TMP/ag" "$BIN" crictl pods -o json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null || echo 99)"
    [[ "$CNT" == "0" ]] && { G25C=1; break; }
    sleep 0.5
  done
  if [[ "$G25C" -eq 1 ]]; then
    ok "G25  deployment delete tears pods down to zero sandboxes  (kubelet, T4.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G25c teardown left sandboxes (count=$CNT; agent log: $G25AG)"
    FAIL=$((FAIL+1))
  fi
  kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; SRV2=""
  kill -TERM "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; AGENT_PID=""
  purge_run_shims "$G25TMP"
  rm -rf "$G25TMP" "$G25SRV" "$G25AG" 2>/dev/null || true
fi

# --- T4.2 Scope B: probes, volumes, configMap + hostname (G26) ---
# G26 reuses G25's airgap recipe but runs the Sprint 18 echo workload
# (build-echo-image.sh) so the kubelet's S1 probe engine is exercised end
# to end over real containerd: readiness gates Running+Ready (G26a);
# /fail flips /healthz to 500 and liveness restarts the container (G26b);
# /data/boot.txt survives container restarts via emptyDir (G26c); /etc/
# config projection + the pod-name hostname (S4) are asserted from inside
# the container (G26d). Same SKIP gates as G25: no vendored containerd,
# or no cc for the airgap images (Q27).
if [[ -z "$G24_VENDOR_BIN" ]]; then
  echo "SKIP G26 kubelet probes/volumes end-to-end (no vendored containerd)"
elif ! command -v cc >/dev/null 2>&1; then
  echo "SKIP G26 kubelet probes/volumes end-to-end (no cc for the airgap images, Q27)"
else
  G26TMP="$(mktemp -d)"; G26SRV="$(mktemp)"; G26AG="$(mktemp)"
  G26_PORT="$(pick_port)"
  "$BIN" server --data-dir "$G26TMP/srv" --bind-address 127.0.0.1 --https-listen-port "$G26_PORT" >"$G26SRV" 2>&1 &
  SRV2=$!
  for _ in $(seq 1 60); do grep -q "discovery listening" "$G26SRV" && break; sleep 0.1; done
  BASE26="http://127.0.0.1:${G26_PORT}"
  PAUSE_REF="init-pro.local/pause:0.1"
  G26_ECHO="$G26TMP/echo.tar"
  "$SCRIPT_DIR/build-pause-image.sh" "$G26TMP/pause.tar" "$PAUSE_REF" >/dev/null 2>&1 || true
  "$SCRIPT_DIR/build-echo-image.sh" "$G26_ECHO" "init-pro.local/echo:0.1" >/dev/null || true
  INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" "$BIN" agent --data-dir "$G26TMP/ag" --token dev \
      --server "$BASE26" --node-name g26-node >"$G26AG" 2>&1 &
  AGENT_PID=$!
  for _ in $(seq 1 300); do grep -q "containerd healthy" "$G26AG" && break; sleep 0.1; done
  INIT_PRO_DATA_DIR="$G26TMP/ag" "$BIN" ctr -n k8s.io images import "$G26TMP/pause.tar" >/dev/null 2>&1 || true
  INIT_PRO_DATA_DIR="$G26TMP/ag" "$BIN" ctr -n k8s.io images import "$G26_ECHO" >/dev/null 2>&1 || true
  curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
    -d '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"g26-cm","namespace":"default"},"data":{"greeting":"hello-from-cm"}}' \
    "$BASE26/api/v1/namespaces/default/configmaps"
  curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
    -d '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"g26-dep","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"g26"}},"template":{"metadata":{"labels":{"app":"g26"}},"spec":{"containers":[{"name":"echo","image":"init-pro.local/echo:0.1","readinessProbe":{"httpGet":{"path":"/healthz","port":8080},"initialDelaySeconds":1,"periodSeconds":2,"failureThreshold":3},"livenessProbe":{"httpGet":{"path":"/healthz","port":8080},"initialDelaySeconds":3,"periodSeconds":2,"failureThreshold":3},"volumeMounts":[{"name":"data","mountPath":"/data"},{"name":"cfg","mountPath":"/etc/config","readOnly":true}]}],"volumes":[{"name":"data","emptyDir":{}},{"name":"cfg","configMap":{"name":"g26-cm"}}]}}}}' \
    "$BASE26/apis/apps/v1/namespaces/default/deployments"
  # G26a: pod becomes Ready ONLY when the kubelet's readiness probe on
  # /healthz succeeds (S1 engine over real containerd, first e2e proof).
  G26A=0
  for _ in $(seq 1 240); do
    S="$(curl -s "$BASE26/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for p in items:
  if p.get("metadata",{}).get("labels",{}).get("app")!="g26": continue
  st=p.get("status",{})
  conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
  if st.get("phase")=="Running" and conds.get("Ready")=="True": print("READY"); break' 2>/dev/null || true)"
    [[ "$S" == "READY" ]] && { G26A=1; break; }
    sleep 0.5
  done
  if [[ "$G26A" -eq 1 ]]; then
    ok "G26  readiness probe gates pod Running+Ready  (kubelet probes, T4.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G26a pod never Ready (readiness probe; server: $G26SRV agent: $G26AG)"
    FAIL=$((FAIL+1))
  fi
  # G26b: /fail plants /healthz-fail (ephemeral rootfs) -> liveness probe
  # sees 500 -> kubelet restarts the container -> marker gone, Ready again.
  G26B=0; PODIP=""
  if [[ "$G26A" -eq 1 ]]; then
    for _ in $(seq 1 60); do
      PODIP="$(curl -s "$BASE26/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for p in items:
  if p.get("metadata",{}).get("labels",{}).get("app")!="g26": continue
  ip=p.get("status",{}).get("podIP","")
  if ip: print(ip); break' 2>/dev/null || true)"
      [[ -n "$PODIP" ]] && break
      sleep 0.5
    done
    if [[ -n "$PODIP" ]]; then
      FAILED=0
      for _ in $(seq 1 12); do
        CODE="$(curl -s --max-time 5 -o /dev/null -w '%{http_code}' "http://$PODIP:8080/fail" 2>/dev/null || true)"
        [[ "$CODE" == "200" ]] && { FAILED=1; break; }
        sleep 0.5
      done
      if [[ "$FAILED" -eq 1 ]]; then
        RC=0
        for _ in $(seq 1 120); do
          RC="$(curl -s "$BASE26/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(1)
rc=0
for p in items:
  if p.get("metadata",{}).get("labels",{}).get("app")!="g26": continue
  cs=p.get("status",{}).get("containerStatuses",[])
  if cs: rc=cs[0].get("restartCount",0)
  break
print(rc)' 2>/dev/null || echo 0)"
          [[ "${RC:-0}" -ge 1 ]] && break
          sleep 0.5
        done
        if [[ "${RC:-0}" -ge 1 ]]; then
          HZ=0
          for _ in $(seq 1 120); do
            CODE="$(curl -s --max-time 5 -o /dev/null -w '%{http_code}' "http://$PODIP:8080/healthz" 2>/dev/null || true)"
            if [[ "$CODE" == "200" ]] && curl -s "$BASE26/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(1)
for p in items:
  if p.get("metadata",{}).get("labels",{}).get("app")!="g26": continue
  st=p.get("status",{})
  conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
  raise SystemExit(0 if st.get("phase")=="Running" and conds.get("Ready")=="True" else 1)
sys.exit(1)' 2>/dev/null; then
              HZ=1; break
            fi
            sleep 0.5
          done
          [[ "$HZ" -eq 1 ]] && G26B=1
        fi
      fi
    fi
  fi
  if [[ "$G26B" -eq 1 ]]; then
    ok "G26  liveness probe restarts container after /fail -> 500  (kubelet probes, T4.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G26b liveness restart (podIP=${PODIP:-?} rc=${RC:-0} hz=${HZ:-0}; agent log: $G26AG)"
    FAIL=$((FAIL+1))
  fi
  # G26c: emptyDir /data persists across restarts. Boots so far: #1 the
  # original container, #2 the G26b liveness restart, #3 this kill -> the
  # echo server must answer /bootcount == 3 (host dir survives, sandbox
  # stays).
  G26C=0
  if [[ "$G26B" -eq 1 ]]; then
    CID="$(INIT_PRO_DATA_DIR="$G26TMP/ag" "$BIN" crictl ps -q 2>/dev/null | head -1)"
    if [[ -n "$CID" ]]; then
      INIT_PRO_DATA_DIR="$G26TMP/ag" "$BIN" crictl stop --timeout 5 "$CID" >/dev/null 2>&1 || true
      for _ in $(seq 1 120); do
        BC="$(curl -s --max-time 5 "http://$PODIP:8080/bootcount" 2>/dev/null || true)"
        [[ "$BC" == "3" ]] && { G26C=1; break; }
        sleep 0.5
      done
    fi
  fi
  if [[ "$G26C" -eq 1 ]]; then
    ok "G26  emptyDir /data/bootcount=3 across container restarts  (kubelet volumes, T4.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G26c emptyDir bootcount (bc=${BC:-?}; agent log: $G26AG)"
    FAIL=$((FAIL+1))
  fi
  # G26d: /etc/config projection (CONFIG greeting hello-from-cm) and the
  # S4 hostname passthrough (ECHO <pod name>), read from inside the pod.
  G26D=0
  if [[ "$G26A" -eq 1 ]]; then
    if [[ -z "$PODIP" ]]; then
      for _ in $(seq 1 60); do
        PODIP="$(curl -s "$BASE26/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for p in items:
  if p.get("metadata",{}).get("labels",{}).get("app")!="g26": continue
  ip=p.get("status",{}).get("podIP","")
  if ip: print(ip); break' 2>/dev/null || true)"
        [[ -n "$PODIP" ]] && break
        sleep 0.5
      done
    fi
    if [[ -n "$PODIP" ]]; then
      PODNAME="$(curl -s "$BASE26/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for p in items:
  if p.get("metadata",{}).get("labels",{}).get("app")!="g26": continue
  nm=p.get("metadata",{}).get("name","")
  if nm: print(nm); break' 2>/dev/null || true)"
      if [[ -n "$PODNAME" ]]; then
        BODY="$(curl -s --max-time 5 "http://$PODIP:8080/" 2>/dev/null || true)"
        if [[ "$BODY" == *"CONFIG greeting hello-from-cm"* && "$BODY" == *"ECHO $PODNAME"* ]]; then
          G26D=1
        fi
      fi
    fi
  fi
  if [[ "$G26D" -eq 1 ]]; then
    ok "G26  configMap projection + hostname==pod name in container  (kubelet volumes, T4.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G26d configMap/hostname (pod=${PODNAME:-?} ip=${PODIP:-?}; agent log: $G26AG)"
    FAIL=$((FAIL+1))
  fi
  curl -s -o /dev/null -X DELETE "$BASE26/apis/apps/v1/namespaces/default/deployments/g26-dep" || true
  # Bounded pod-gone wait (G26 tail): the fire-and-forget DELETE returns long
  # before the kubelet finishes teardown, and killing the agent while its
  # containerd shims still hold the emptyDir/log mounts makes the rm -rf below
  # hit EBUSY — which set -euo pipefail turns into a hard abort that would
  # skip G31 and the argoproj suites. Poll <=20s for zero pods labelled
  # app=g26 before killing SRV2/AGENT_PID (errors ignored: never fatal).
  for _ in $(seq 1 40); do
    G26_LEFT="$(curl -s "$BASE26/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for p in items:
  if p.get("metadata",{}).get("labels",{}).get("app")=="g26": print("LEFT"); break' 2>/dev/null || true)"
    [[ "$G26_LEFT" != "LEFT" ]] && break
    sleep 0.5
  done
  kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; SRV2=""
  kill -TERM "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; AGENT_PID=""
  purge_run_shims "$G26TMP"
  rm -rf "$G26TMP" "$G26SRV" "$G26AG" 2>/dev/null || true
fi

# --- T4.2 Scope C: kubectl logs/exec data plane over the apiserver relay ---
# G30 closes T4.2 Scope C: the logs/exec verbs ride the upstream-shaped
# chain kubectl -> apiserver relay (pod->nodeName->node InternalIP +
# init-pro.io/kubelet-port annotation) -> kubelet local listener ->
# crictl subprocess (Q26/Q31). Plain HTTP single-shot relay; stdout+stderr
# are MERGED by design in v1 (Q31 documents the deviation from upstream's
# SPDY/WebSocket byte-splitting). Same vendor+cc gates as G25/G26 since the
# workload is the Q27 airgap echo image — which now also ships /bin/wfstep
# (build-echo-image.sh, same source as build-workflow-image.sh).
if [[ -z "$G24_VENDOR_BIN" ]]; then
  echo "SKIP G30 kubelet logs/exec relay (no vendored containerd)"
elif ! command -v cc >/dev/null 2>&1; then
  echo "SKIP G30 kubelet logs/exec relay (no cc for the airgap echo image)"
else
  G30TMP="$(mktemp -d)"; G30SRV="$(mktemp)"; G30AG="$(mktemp)"
  G30_PORT="$(pick_port)"; G30_KPORT="$(pick_port)"
  "$BIN" server --data-dir "$G30TMP/srv" --bind-address 127.0.0.1 --https-listen-port "$G30_PORT" >"$G30SRV" 2>&1 &
  SRV2=$!
  for _ in $(seq 1 60); do grep -q "discovery listening" "$G30SRV" && break; sleep 0.1; done
  BASE30="http://127.0.0.1:${G30_PORT}"
  PAUSE_REF="init-pro.local/pause:0.1"
  G30_ECHO_TAR="$G30TMP/echo.tar"
  ECHO_REF="init-pro.local/echo:0.1"
  "$SCRIPT_DIR/build-echo-image.sh" "$G30_ECHO_TAR" "$ECHO_REF" >/dev/null 2>&1 || true
  # Explicit --node-ip/--kubelet-port prove the flag wiring AND force the
  # node-annotation addressing path (the relay must NOT rely on a lucky
  # well-known 10250 here — the port is random).
  INIT_PRO_SANDBOX_IMAGE="$PAUSE_REF" "$BIN" agent --data-dir "$G30TMP/ag" --token dev \
      --server "$BASE30" --node-name g30-node \
      --node-ip 127.0.0.1 --kubelet-port "$G30_KPORT" >"$G30AG" 2>&1 &
  AGENT_PID=$!
  for _ in $(seq 1 300); do grep -q "containerd healthy" "$G30AG" && break; sleep 0.1; done
  G30_PAUSE_TAR="$G30TMP/pause.tar"
  "$SCRIPT_DIR/build-pause-image.sh" "$G30_PAUSE_TAR" "$PAUSE_REF" >/dev/null 2>&1 || true
  INIT_PRO_DATA_DIR="$G30TMP/ag" "$BIN" ctr -n k8s.io images import "$G30_PAUSE_TAR" >/dev/null 2>&1 || true
  INIT_PRO_DATA_DIR="$G30TMP/ag" "$BIN" ctr -n k8s.io images import "$G30_ECHO_TAR" >/dev/null 2>&1 || true

  curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
    -d '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"g30-dep","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"g30"}},"template":{"metadata":{"labels":{"app":"g30"}},"spec":{"containers":[{"name":"echo","image":"'"$ECHO_REF"'"}]}}}}' \
    "$BASE30/apis/apps/v1/namespaces/default/deployments"
  G30A=0; G30POD=""
  for _ in $(seq 1 240); do
    S="$(curl -s "$BASE30/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for p in items:
  if p.get("metadata",{}).get("labels",{}).get("app")!="g30": continue
  st=p.get("status",{})
  conds={c.get("type"):c.get("status") for c in st.get("conditions",[])}
  if st.get("phase")=="Running" and conds.get("Ready")=="True": print(p["metadata"]["name"]); break' 2>/dev/null || true)"
    [[ -n "$S" ]] && { G30A=1; G30POD="$S"; break; }
    sleep 0.5
  done
  if [[ "$G30A" -eq 1 ]]; then
    # Generate deterministic log noise: the echo server emits ONE stderr
    # line per accepted connection (no request-path logging), captured
    # into the CRI log file.
    G30IP="$(curl -s "$BASE30/api/v1/namespaces/default/pods/$G30POD" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"].get("podIP",""))' 2>/dev/null || true)"
    if [[ -z "${G30IP:-}" ]]; then
      for _ in $(seq 1 60); do
        G30IP="$(curl -s "$BASE30/api/v1/namespaces/default/pods/$G30POD" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"].get("podIP",""))' 2>/dev/null || true)"
        [[ -n "$G30IP" ]] && break
        sleep 0.5
      done
    fi
    curl -s --max-time 5 -o /dev/null "http://$G30IP:8080/noise1" 2>/dev/null || true
    curl -s --max-time 5 -o /dev/null "http://$G30IP:8080/noise2" 2>/dev/null || true
    sleep 1
  else
    echo "FAIL G30 pod never Running+Ready (server: $G30SRV agent: $G30AG)"
    FAIL=$((FAIL+1))
  fi

  ln -sf "$BIN" "$G30TMP/kubectl"
  export INIT_PRO_SERVER="$BASE30"   # exercises the env-var --server fallback

  # G30a: kubectl logs (one-shot) relays the streamed CRI log bytes.
  G30B_LOG=""
  if [[ "$G30A" -eq 1 ]]; then
    G30B_LOG="$("$G30TMP/kubectl" logs "$G30POD" -n default 2>/dev/null || true)"
    if printf '%s' "$G30B_LOG" | grep -q "echo: connection from"; then
      ok "G30  kubectl logs relays container log lines via kubelet  (T4.2 Scope C, Q31)"
      PASS=$((PASS+1))
    else
      echo "FAIL G30a kubectl logs missing generated line (got: $(printf '%s' "$G30B_LOG" | head -3))"
      FAIL=$((FAIL+1))
    fi
  fi

  # G30b: kubectl exec exit-0 + output passthrough (/bin/wfstep hello).
  G30C_OK=0
  if [[ "$G30A" -eq 1 ]]; then
    G30X_OUT="$("$G30TMP/kubectl" exec "$G30POD" -n default -- /bin/wfstep hello 2>/dev/null)" && G30C_RC=0 || G30C_RC=$?
    if [[ "$G30C_RC" -eq 0 && "$G30X_OUT" == "wfstep: hello" ]]; then
      ok "G30  kubectl exec runs /bin/wfstep: rc0 + merged output  (T4.2 Scope C, Q31)"
      PASS=$((PASS+1))
    else
      echo "FAIL G30b exec success (rc=$G30C_RC out='$G30X_OUT')"
      FAIL=$((FAIL+1))
    fi
  fi

  # G30b2: -f streams NEW lines as they appear (client-side polling
  # follow, Q21 cadence): snapshot after the first poll, generate fresh
  # traffic, then require the follow output to have grown past it.
  if [[ "$G30A" -eq 1 ]]; then
    "$G30TMP/kubectl" logs "$G30POD" -n default -f >"$G30TMP/follow.out" 2>/dev/null &
    G30F_PID=$!
    sleep 2   # >= two follow ticks so the baseline snapshot is taken
    G30F_BASE="$(wc -l <"$G30TMP/follow.out")"
    G30IP2="$(curl -s "$BASE30/api/v1/namespaces/default/pods/$G30POD" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"].get("podIP",""))' 2>/dev/null || true)"
    curl -s --max-time 5 -o /dev/null "http://$G30IP2:8080/followme" 2>/dev/null || true
    sleep 3   # >= several follow ticks to observe the appended line
    G30F_NOW="$(wc -l <"$G30TMP/follow.out")"
    kill "$G30F_PID" 2>/dev/null || true; wait "$G30F_PID" 2>/dev/null || true
    if [[ "${G30F_NOW:-0}" -gt "${G30F_BASE:-0}" ]]; then
      ok "G30  kubectl logs -f streams appended lines (client poll follow)  (T4.2 Scope C, Q31)"
      PASS=$((PASS+1))
    else
      echo "FAIL G30b2 logs -f did not grow ($G30F_BASE -> $G30F_NOW)"
      FAIL=$((FAIL+1))
    fi
  fi

  # G30c: kubectl exec propagates the remote nonzero exit (rc 1) —
  # /bin/wfstep --fail prints to the merged stream and exits 1.
  G30D_OK=0
  if [[ "$G30A" -eq 1 ]]; then
    G30F_OUT="$("$G30TMP/kubectl" exec "$G30POD" -n default -- /bin/wfstep --fail oops 2>/dev/null)" && G30F_RC=0 || G30F_RC=$?
    if [[ "$G30F_RC" -eq 1 ]] && printf '%s' "$G30F_OUT" | grep -q "wfstep: oops"; then
      ok "G30  kubectl exec propagates remote exit 1 (+output)  (T4.2 Scope C, Q31)"
      PASS=$((PASS+1))
    else
      echo "FAIL G30c exec failure propagation (rc=$G30F_RC out='$G30F_OUT')"
      FAIL=$((FAIL+1))
    fi
  fi

  unset INIT_PRO_SERVER
  curl -s -o /dev/null -X DELETE "$BASE30/apis/apps/v1/namespaces/default/deployments/g30-dep" || true
  # Same bounded pod-gone wait as the G26 tail (label app=g30): do not race
  # the kubelet/containerd teardown into the rm -rf below (<=20s, errors
  # ignored so the wait itself can never abort the script).
  for _ in $(seq 1 40); do
    G30_LEFT="$(curl -s "$BASE30/api/v1/namespaces/default/pods" | python3 -c 'import json,sys
try: items=json.load(sys.stdin).get("items",[])
except Exception: sys.exit(0)
for p in items:
  if p.get("metadata",{}).get("labels",{}).get("app")=="g30": print("LEFT"); break' 2>/dev/null || true)"
    [[ "$G30_LEFT" != "LEFT" ]] && break
    sleep 0.5
  done
  kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; SRV2=""
  kill -TERM "$AGENT_PID" 2>/dev/null || true; wait "$AGENT_PID" 2>/dev/null || true; AGENT_PID=""
  purge_run_shims "$G30TMP"
  rm -rf "$G30TMP" "$G30SRV" "$G30AG" 2>/dev/null || true
fi
