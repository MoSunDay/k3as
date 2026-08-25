# Scheduler suite (G22-G23): T3.2 kube-scheduler placement +
# Unschedulable + HTTP extender filter steering.
# Sourced by golden-conformance.sh; runs in the main script's shell.
# --- T3.2: kube-scheduler placement + unschedulable (G22/G23) ---
# G22: default plugins through the live server. Two nodes (one labeled
# disk=ssd); a nodeSelector pod must land on the labeled node and carry
# PodScheduled=True; an impossible selector must leave PodScheduled=False
# with reason=Unschedulable (no hot loop -- condition settles once).
TMP_G22NA="$(mktemp)"; TMP_G22NB="$(mktemp)"; TMP_G22P="$(mktemp)"; TMP_G22Q="$(mktemp)"
printf '%s' '{"apiVersion":"v1","kind":"Node","metadata":{"name":"g22-a","labels":{"disk":"ssd"}}}' > "$TMP_G22NA"
printf '%s' '{"apiVersion":"v1","kind":"Node","metadata":{"name":"g22-b"}}' > "$TMP_G22NB"
printf '%s' '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g22-fit","namespace":"default"},"spec":{"nodeSelector":{"disk":"ssd"},"containers":[{"name":"c","image":"pause"}]}}' > "$TMP_G22P"
printf '%s' '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g22-nofit","namespace":"default"},"spec":{"nodeSelector":{"disk":"nowhere"},"containers":[{"name":"c","image":"pause"}]}}' > "$TMP_G22Q"
G22_NA="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G22NA" "$BASE/api/v1/nodes")"
G22_NB="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G22NB" "$BASE/api/v1/nodes")"
G22_P="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G22P" "$BASE/api/v1/namespaces/default/pods")"
G22_Q="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" --data-binary "@$TMP_G22Q" "$BASE/api/v1/namespaces/default/pods")"
G22_BOUND=0
for _ in $(seq 1 80); do
  N="$(curl -s "$BASE/api/v1/namespaces/default/pods/g22-fit" | python3 -c "import json,sys; print(json.load(sys.stdin).get('spec',{}).get('nodeName',''))" 2>/dev/null || true)"
  if [[ "$N" == "g22-a" ]]; then G22_BOUND=1; break; fi
  sleep 0.25
done
G22_SCHED_TRUE=0
curl -s "$BASE/api/v1/namespaces/default/pods/g22-fit" | python3 -c "
import json,sys
cs=json.load(sys.stdin).get('status',{}).get('conditions',[])
raise SystemExit(0 if any(c.get('type')=='PodScheduled' and c.get('status')=='True' for c in cs) else 1)" 2>/dev/null && G22_SCHED_TRUE=1
G22_UNSCHED=0
for _ in $(seq 1 80); do
  if curl -s "$BASE/api/v1/namespaces/default/pods/g22-nofit" | python3 -c "
import json,sys
cs=json.load(sys.stdin).get('status',{}).get('conditions',[])
raise SystemExit(0 if any(c.get('type')=='PodScheduled' and c.get('status')=='False' and c.get('reason')=='Unschedulable' for c in cs) else 1)" 2>/dev/null; then
    G22_UNSCHED=1; break
  fi
  sleep 0.25
done
if [[ "$G22_NA" == "201" && "$G22_NB" == "201" && "$G22_P" == "201" && "$G22_Q" == "201" \
      && "$G22_BOUND" -eq 1 && "$G22_SCHED_TRUE" -eq 1 && "$G22_UNSCHED" -eq 1 ]]; then
  ok "G22  scheduler nodeSelector placement + PodScheduled=True + Unschedulable  (scheduler, T3.2)"
  PASS=$((PASS+1))
else
  echo "FAIL G22  scheduler placement (na=$G22_NA nb=$G22_NB p=$G22_P q=$G22_Q bound=$G22_BOUND true=$G22_SCHED_TRUE unsched=$G22_UNSCHED)"
  FAIL=$((FAIL+1))
fi
rm -f "$TMP_G22NA" "$TMP_G22NB" "$TMP_G22P" "$TMP_G22Q"

# G23: the HTTP extender seam (Q3/Q23). A python3 stub extender rejects
# g23-a in its filter verb; a second server booted with
# `--kube-scheduler-arg config=<KubeSchedulerConfiguration>` must place the
# pod on the stub-approved node only.
G23=0
if ! command -v python3 >/dev/null 2>&1; then
  echo "FAIL G23  python3 required for the stub extender"
  FAIL=$((FAIL+1))
else
  STUB_PORT="$(pick_port)"
  SRV2_PORT="$(pick_port)"
  TMP_G23STUB="$(mktemp)"; TMP_G23CFG="$(mktemp)"; DD2="$(mktemp -d)"; LOG2="$(mktemp)"
  cat > "$TMP_G23STUB" <<'PYSTUB'
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(n) or b"{}")
        items = body.get("nodes", {}).get("Items", [])
        names = [i.get("metadata", {}).get("name", "") for i in items]
        if self.path == "/filter":
            resp = {"NodeNames": [x for x in names if x == "g23-b"]}
        else:
            resp = [{"host": x, "score": 100 if x == "g23-b" else 0} for x in names]
        data = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def log_message(self, *a):
        pass

HTTPServer(("127.0.0.1", int(__import__("sys").argv[1])), H).serve_forever()
PYSTUB
  python3 "$TMP_G23STUB" "$STUB_PORT" >/dev/null 2>&1 &
  STUB_PID=$!
  printf '{"apiVersion":"kubescheduler.config.k8s.io/v1","kind":"KubeSchedulerConfiguration","extenders":[{"urlPrefix":"http://127.0.0.1:%s","filterVerb":"filter","prioritizeVerb":"prioritize","weight":1,"ignorable":false}]}' "$STUB_PORT" > "$TMP_G23CFG"
  "$BIN" server --data-dir "$DD2" --bind-address 127.0.0.1 --https-listen-port "$SRV2_PORT" \
    --kube-scheduler-arg "config=$TMP_G23CFG" >"$LOG2" 2>&1 &
  SRV2=$!
  BASE2="http://127.0.0.1:${SRV2_PORT}"
  for _ in $(seq 1 60); do grep -q "discovery listening" "$LOG2" && break; sleep 0.1; done
  if grep -q "scheduler extenders loaded" "$LOG2"; then
    curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
      -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"g23-a"}}' "$BASE2/api/v1/nodes"
    curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
      -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"g23-b"}}' "$BASE2/api/v1/nodes"
    curl -s -o /dev/null -X POST -H "Content-Type: application/json" \
      -d '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g23-p","namespace":"default"},"spec":{"containers":[{"name":"c","image":"pause"}]}}' "$BASE2/api/v1/namespaces/default/pods"
    for _ in $(seq 1 80); do
      N="$(curl -s "$BASE2/api/v1/namespaces/default/pods/g23-p" | python3 -c "import json,sys; print(json.load(sys.stdin).get('spec',{}).get('nodeName',''))" 2>/dev/null || true)"
      if [[ "$N" == "g23-b" ]]; then G23=1; break; fi
      if [[ "$N" == "g23-a" ]]; then break; fi
      sleep 0.25
    done
  fi
  if [[ "$G23" -eq 1 ]]; then
    ok "G23  extender filter steers placement via --kube-scheduler-arg config  (scheduler, T3.2)"
    PASS=$((PASS+1))
  else
    echo "FAIL G23  extender placement (check $LOG2)"
    FAIL=$((FAIL+1))
  fi
  kill "$SRV2" 2>/dev/null || true; wait "$SRV2" 2>/dev/null || true; SRV2=""
  kill "$STUB_PID" 2>/dev/null || true; wait "$STUB_PID" 2>/dev/null || true; STUB_PID=""
  rm -f "$TMP_G23STUB" "$TMP_G23CFG"; rm -rf "$DD2" "$LOG2"
fi
