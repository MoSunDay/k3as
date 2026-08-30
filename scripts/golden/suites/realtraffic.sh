# Ingress REST-to-real-TCP suite (B3): REST-created Ingress + Service +
# manual Endpoints -> real TCP through the built-in Router's SERVED gateway
# listeners, then empty-backend 503 (ST4/ST5 assertion style).
# Sourced by golden-conformance.sh; runs in the main script's shell
# (helpers: check_body/check_status/check_method/ok, counters PASS/FAIL,
# $BASE, $BIN, pick_port; `set -euo pipefail` is already on).
# Owned by plan item: B3. Check-ID range reserved: G70-G72. Hard cap 400 lines.
#
# ---------------------------------------------------------------------------
# B3 INVESTIGATION VERDICT (this round; ledgered as FINDING (a) in the round
# report): the Ingress HOST-routing plane is NOT listener-served by
# `init-pro server` today. Code-path evidence:
#   - crates/cli/src/runtime.rs wires the service plane with
#     `router::NodePortConfig::default()`; `Default for NodePortConfig` sets
#     `ingress_listen: None` (crates/router/src/nodeport.rs).
#   - nodeport.rs `reconcile_loop` spawns the openrusty Ingress gateway
#     (`crate::gateway::run`, which compiles networking.k8s.io/v1 Ingress
#     objects into host x path routes via ingress.rs + gateway_config.rs)
#     ONLY when `config.ingress_listen` is `Some`.
#   - No CLI flag/env/other caller sets `ingress_listen`; only
#     crates/router/tests/ingress_gateway.rs builds it, in-process.
#   - The Ingress REFLECTOR does run in the server (router::supervise ->
#     ingress_watch folds Ingress objects into the resolver), so REST-created
#     Ingress objects are folded -- but no listener consumes the compiled
#     host-routed table. The only HTTP listener the server binds is the
#     apiserver, which answers 404 on non-API paths (G70 pins that, too).
# Per the round rules these checks are NOT faked into a pass: G70-G72 drive
# the strongest REAL assertions available on the plane that IS served --
# the per-nodePort openrusty gateway listeners (ST1-ST6 plane, Q28/Q32) --
# with Ingress-shaped REST input (Ingress + selector-less Services + manual
# Endpoints, which the endpoints controller never overwrites: selector-less
# Services return early in crates/controllers/src/controllers/endpoints.rs).
# ---------------------------------------------------------------------------
command -v python3 >/dev/null || { echo "SKIP B3 (python3 required for the local backend + JSON assertions)"; return 0; }

# --- local hygiene: two throwaway HTTP backends writing per-request markers
# (G25-style: everything this suite spawns dies in-suite, never globally). ---
rt_free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null || true
}
# A free port inside the k8s nodePort range (the apiserver validates
# spec.ports[].nodePort against 30000-32767, crates/apiserver/src/service.rs).
rt_free_nodeport() {
  python3 - "$1" "$2" <<'PY' 2>/dev/null || true
import socket, sys
lo, hi = int(sys.argv[1]), int(sys.argv[2])
for p in range(lo, hi):
    s = socket.socket()
    try:
        s.bind(("0.0.0.0", p)); s.close(); print(p); break
    except OSError:
        s.close()
PY
}
RT_NP1="$(rt_free_nodeport 31370 31390)"; RT_NP2="$(rt_free_nodeport 31391 31410)"
RT_B1="$(rt_free_port)"; RT_B2="$(rt_free_port)"
RT_MK1="$(mktemp)"; RT_MK2="$(mktemp)"; RT_BLOG="$(mktemp)"
cat > "$RT_BLOG.py" <<'PY'
import http.server, socketserver, sys
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        with open(sys.argv[2], "a") as f: f.write(self.path + "\n")
        body = b"golden-realtraffic"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
class S(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
S(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY

RT_BACKENDS_OK=0
if [[ -n "$RT_NP1" && -n "$RT_NP2" && -n "$RT_B1" && -n "$RT_B2" ]]; then
  python3 "$RT_BLOG.py" "$RT_B1" "$RT_MK1" >/dev/null 2>&1 & RT_P1=$!
  python3 "$RT_BLOG.py" "$RT_B2" "$RT_MK2" >/dev/null 2>&1 & RT_P2=$!
  for _ in $(seq 1 20); do
    RC=0
    curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$RT_B1/rt-boot" 2>/dev/null || RC=$?
    [[ "$RC" == "0" ]] && { RT_BACKENDS_OK=1; break; }
    sleep 0.25
  done
fi
if [[ "$RT_BACKENDS_OK" -ne 1 ]]; then
  [[ -n "${RT_P1:-}" ]] && kill "$RT_P1" 2>/dev/null || true
  rm -f "$RT_MK1" "$RT_MK2" "$RT_BLOG" "$RT_BLOG.py" 2>/dev/null || true
  echo "SKIP B3 (local backends could not start on this host; not a conformance break)"
  return 0
fi

# --- REST setup (uncounted; the three numbered checks below assert the
# wire-visible outcomes). JSON-only wire, Q10. Selector-less Services so the
# endpoints controller never rewrites the manual Endpoints. ---
RT_NS="default"
rt_post() {
  curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST \
    -H 'Content-Type: application/json' --data-binary "$2" "$BASE$1" 2>/dev/null || true
}
RT_SVC1="$(rt_post "/api/v1/namespaces/$RT_NS/services" "{\"apiVersion\":\"v1\",\"kind\":\"Service\",\"metadata\":{\"name\":\"rt-svc\",\"namespace\":\"$RT_NS\"},\"spec\":{\"type\":\"NodePort\",\"ports\":[{\"name\":\"web\",\"port\":80,\"targetPort\":$RT_B1,\"protocol\":\"TCP\",\"nodePort\":$RT_NP1}]}}")"
RT_SVC2="$(rt_post "/api/v1/namespaces/$RT_NS/services" "{\"apiVersion\":\"v1\",\"kind\":\"Service\",\"metadata\":{\"name\":\"rt-svc2\",\"namespace\":\"$RT_NS\"},\"spec\":{\"type\":\"NodePort\",\"ports\":[{\"name\":\"web\",\"port\":80,\"targetPort\":$RT_B2,\"protocol\":\"TCP\",\"nodePort\":$RT_NP2}]}}")"
RT_EP1="$(rt_post "/api/v1/namespaces/$RT_NS/endpoints" "{\"apiVersion\":\"v1\",\"kind\":\"Endpoints\",\"metadata\":{\"name\":\"rt-svc\",\"namespace\":\"$RT_NS\"},\"subsets\":[{\"addresses\":[{\"ip\":\"127.0.0.1\"}],\"ports\":[{\"name\":\"web\",\"port\":$RT_B1,\"protocol\":\"TCP\"}]}]}")"
RT_EP2="$(rt_post "/api/v1/namespaces/$RT_NS/endpoints" "{\"apiVersion\":\"v1\",\"kind\":\"Endpoints\",\"metadata\":{\"name\":\"rt-svc2\",\"namespace\":\"$RT_NS\"},\"subsets\":[{\"addresses\":[{\"ip\":\"127.0.0.1\"}],\"ports\":[{\"name\":\"web\",\"port\":$RT_B2,\"protocol\":\"TCP\"}]}]}")"
RT_IG1="$(rt_post "/apis/networking.k8s.io/v1/namespaces/$RT_NS/ingresses" "{\"apiVersion\":\"networking.k8s.io/v1\",\"kind\":\"Ingress\",\"metadata\":{\"name\":\"rt-ing\",\"namespace\":\"$RT_NS\"},\"spec\":{\"rules\":[{\"host\":\"golden.ingress.local\",\"http\":{\"paths\":[{\"path\":\"/\",\"pathType\":\"Prefix\",\"backend\":{\"service\":{\"name\":\"rt-svc\",\"port\":{\"number\":80}}}}]}},{\"host\":\"golden2.ingress.local\",\"http\":{\"paths\":[{\"path\":\"/\",\"pathType\":\"Prefix\",\"backend\":{\"service\":{\"name\":\"rt-svc2\",\"port\":{\"number\":80}}}}]}}]}}")"

RT_G70=0; RT_G71=0; RT_G72=0
RT_IG_BODY="$(curl -s --max-time 5 "$BASE/apis/networking.k8s.io/v1/namespaces/$RT_NS/ingresses/rt-ing" 2>/dev/null || true)"
RT_HH_CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -H 'Host: golden.ingress.local' "$BASE/" 2>/dev/null || true)"
RT_HH_LINES_BEFORE="$(wc -l < "$RT_MK1" 2>/dev/null || echo 0)"

# G70: REST Ingress + Service + manual Endpoints -> REAL TCP through the
# router gateway listener the code actually serves (the per-nodePort
# listener, ST3/ST4 plane): bounded convergence poll -> 200 from the local
# backend + its marker observed. ALSO pins FINDING (a): the same Host
# header against the server's only other HTTP listener (the apiserver on
# $PORT) does NOT host-route (404, backend sees nothing) -- the missing
# host-routing plane is a ledgered gap, asserted here so it cannot regress
# silently in either direction.
if [[ "$RT_SVC1" == "201" && "$RT_EP1" == "201" && "$RT_IG1" == "201" ]] \
   && printf '%s' "$RT_IG_BODY" | grep -q 'golden.ingress.local' \
   && [[ "$RT_HH_CODE" == "404" ]] \
   && [[ "$(wc -l < "$RT_MK1" 2>/dev/null || echo 0)" == "$RT_HH_LINES_BEFORE" ]]; then
  for _ in $(seq 1 40); do
    RT_C="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$RT_NP1/rt-g70" 2>/dev/null || true)"
    [[ "$RT_C" == "200" ]] && { RT_G70=1; break; }
    sleep 0.5
  done
  grep -q '/rt-g70' "$RT_MK1" 2>/dev/null || RT_G70=0
fi
if [[ "$RT_G70" -eq 1 ]]; then
  ok "G70  REST Ingress+Service+manual Endpoints -> real TCP 200 + backend marker on the served nodePort listener (host plane absent: apiserver 404s the Host header = FINDING (a))"
  PASS=$((PASS+1))
else
  echo "FAIL G70  real TCP through the router listener did not converge (svc=$RT_SVC1 ep=$RT_EP1 ing=$RT_IG1 apiserver-host=$RT_HH_CODE nodeport=${RT_C:-000} marker=$(tr '\n' ' ' <"$RT_MK1" 2>/dev/null || true))"
  FAIL=$((FAIL+1))
fi

# G71: empty-backend path (ST5 style): DELETE the Endpoints object -> the
# gate answers 503 while the listener stays bound (Q28 503-while-empty
# parity, now the openrusty gate; crates/router/src/gate.rs).
if [[ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X DELETE "$BASE/api/v1/namespaces/$RT_NS/endpoints/rt-svc" 2>/dev/null || true)" == "200" ]]; then
  for _ in $(seq 1 40); do
    RT_C71="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$RT_NP1/rt-g71" 2>/dev/null || true)"
    [[ "$RT_C71" == "503" ]] && { RT_G71=1; break; }
    sleep 0.5
  done
fi
if [[ "$RT_G71" -eq 1 ]]; then
  ok "G71  DELETE Endpoints -> router listener converges to 503 (empty-backend gate, ST5/Q28)"
  PASS=$((PASS+1))
else
  echo "FAIL G71  nodePort listener never answered 503 after Endpoints delete (last=${RT_C71:-000})"
  FAIL=$((FAIL+1))
fi

# G72: a SECOND route to a DIFFERENT backend port: second Service + manual
# Endpoints + second Ingress host -> its own listener serves 200 with the
# OTHER backend's marker (compile/listener isolation beyond a single rule).
# The host-split itself (golden2.ingress.local vs golden.ingress.local) is
# only COMPILED today (ingress.rs -> RouteTable): it has no served listener
# until FINDING (a) closes, so the host routing is asserted at the REST
# read-back and the multi-backend isolation on the served nodePort plane.
if [[ "$RT_SVC2" == "201" && "$RT_EP2" == "201" ]] \
   && printf '%s' "$RT_IG_BODY" | grep -q 'golden2.ingress.local'; then
  for _ in $(seq 1 40); do
    RT_C72="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$RT_NP2/rt-g72" 2>/dev/null || true)"
    [[ "$RT_C72" == "200" ]] && { RT_G72=1; break; }
    sleep 0.5
  done
  grep -q '/rt-g72' "$RT_MK2" 2>/dev/null || RT_G72=0
  grep -q '/rt-g70' "$RT_MK2" 2>/dev/null && RT_G72=0   # markers must not mix backends
fi
if [[ "$RT_G72" -eq 1 ]]; then
  ok "G72  second Service/Endpoints/Ingress-host -> independent listener 200 + DISTINCT backend marker (host-split compiled, not listener-served: FINDING (a))"
  PASS=$((PASS+1))
else
  echo "FAIL G72  second listener did not serve its own backend (svc=$RT_SVC2 ep=$RT_EP2 code=${RT_C72:-000} mk2=$(tr '\n' ' ' <"$RT_MK2" 2>/dev/null || true))"
  FAIL=$((FAIL+1))
fi

# --- teardown hygiene: nothing this suite created outlives the suite ---
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE/api/v1/namespaces/$RT_NS/services/rt-svc" 2>/dev/null || true
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE/api/v1/namespaces/$RT_NS/services/rt-svc2" 2>/dev/null || true
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE/api/v1/namespaces/$RT_NS/endpoints/rt-svc2" 2>/dev/null || true
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE/apis/networking.k8s.io/v1/namespaces/$RT_NS/ingresses/rt-ing" 2>/dev/null || true
[[ -n "${RT_P1:-}" ]] && kill "$RT_P1" 2>/dev/null || true
[[ -n "${RT_P2:-}" ]] && kill "$RT_P2" 2>/dev/null || true
rm -f "$RT_MK1" "$RT_MK2" "$RT_BLOG" "$RT_BLOG.py" 2>/dev/null || true
