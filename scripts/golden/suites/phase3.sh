# Phase 3 host-direct admission suite (HD1): the api-visible half of the
# host-direct/GPU plane -- admission validation over Pod specs and apps/v1
# Pod templates (crates/api/src/validate.rs, wired into apiserver
# create/apply/replace) plus workload-controller template-annotation
# propagation (RS/DS/STS pod materialization copies
# spec.template.metadata.annotations). IMPLEMENTED semantics; every check
# asserts INTENDED-vs-ACTUAL via status code + a stable message substring.
# Sourced by golden-conformance.sh; runs in the main script's shell
# (helpers: check_body/check_status/check_method/ok, counters PASS/FAIL,
# $BASE, $BIN). Owned by plan item: HD1 (T4.15/Q60 + T7.2/Q61).
# Check-ID range reserved: G90-G98. Hard cap 400 lines.
#
#   G90  Pod: explicit non-upstream imagePullPolicy -> 422 Invalid.
#   G91  Pod: fractional GPU quantity -> 422 (whole-GPU only, Q61).
#   G92  Pod: GPU request > limit -> 422.
#   G93  Deployment: GPU template without init-pro.io/rootfs "/" -> 422
#        (whole-GPU pods are host-direct-only); non-literal values too.
#   G94  Deployment: host-direct template + RollingUpdate (default and
#        explicit) -> 422 (Q60: must not roll alongside predecessors).
#   G95  positive: host-direct Deployment ("/" + Recreate + 1 GPU) -> 201
#        and the ReplicaSet-materialized Pod carries the template's
#        init-pro.io/rootfs annotation verbatim.
#   G96  template branch + other write seams: bad imagePullPolicy inside a
#        Deployment template -> 422; SSA apply and PUT replace both admit.
#   G97  Pod: GPU container with an explicit CUDA_VISIBLE_DEVICES env ->
#        422 Forbidden (platform-injected from the GPU allocation ledger,
#        Q61; S47 review F1).
#   G98  ReplicaSet: GPU template without init-pro.io/rootfs "/" -> 422
#        (the annotation rule now covers all pod templates + bare pods,
#        Q61; S47 review F2).

P3_TMP="$(mktemp -d)"
P3_PODS="/api/v1/namespaces/default/pods"
P3_DEPS="/apis/apps/v1/namespaces/default/deployments"
P3_RSS="/apis/apps/v1/namespaces/default/replicasets"
# Throwaway pods carry schedulerName=golden-none so the in-process scheduler
# cannot bind them mid-check (negative.sh G55 idiom).

# --- G90: explicit invalid imagePullPolicy on a Pod ------------------------
P3_G90='{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g90-pull","namespace":"default"},"spec":{"schedulerName":"golden-none","containers":[{"name":"c","image":"i","imagePullPolicy":"always"}]}}'
P3_G90_CODE="$(curl -s -o "$P3_TMP/g90.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G90" "$BASE$P3_PODS" 2>/dev/null || true)"
P3_G90_BODY="$(cat "$P3_TMP/g90.json" 2>/dev/null || true)"
if [[ "$P3_G90_CODE" == "422" ]] && printf '%s' "$P3_G90_BODY" | grep -qF 'imagePullPolicy: Unsupported value' && printf '%s' "$P3_G90_BODY" | grep -qF 'supported values'; then
  ok "G90  POST pods (imagePullPolicy \"always\") -> 422 Invalid, message pins the upstream value set"
  PASS=$((PASS+1))
else
  echo "FAIL G90  imagePullPolicy probe (code=$P3_G90_CODE body=$P3_G90_BODY; expected 422 + Unsupported value)"
  FAIL=$((FAIL+1))
fi

# --- G91: fractional GPU quantities -> 422 (whole-GPU only, Q61) -----------
P3_G91A='{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g91-gpu-frac","namespace":"default"},"spec":{"schedulerName":"golden-none","containers":[{"name":"c","image":"i","resources":{"limits":{"nvidia.com/gpu":0.5}}}]}}'
P3_G91B='{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g91-gpu-millicore","namespace":"default"},"spec":{"schedulerName":"golden-none","containers":[{"name":"c","image":"i","resources":{"requests":{"amd.com/gpu":"250m"}}}]}}'
P3_G91A_CODE="$(curl -s -o "$P3_TMP/g91a.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G91A" "$BASE$P3_PODS" 2>/dev/null || true)"
P3_G91B_CODE="$(curl -s -o "$P3_TMP/g91b.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G91B" "$BASE$P3_PODS" 2>/dev/null || true)"
P3_G91A_BODY="$(cat "$P3_TMP/g91a.json" 2>/dev/null || true)"
if [[ "$P3_G91A_CODE" == "422" && "$P3_G91B_CODE" == "422" ]] && printf '%s' "$P3_G91A_BODY" | grep -qF 'must be a non-negative integer'; then
  ok "G91  POST pods GPU 0.5 (number) and \"250m\" (string) -> 422 both (whole-GPU only, Q61)"
  PASS=$((PASS+1))
else
  echo "FAIL G91  fractional GPU probe (frac=$P3_G91A_CODE milli=$P3_G91B_CODE body=$P3_G91A_BODY; expected 422 + non-negative integer)"
  FAIL=$((FAIL+1))
fi

# --- G92: GPU request > GPU limit -> 422 ------------------------------------
P3_G92='{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g92-gpu-rl","namespace":"default"},"spec":{"schedulerName":"golden-none","containers":[{"name":"c","image":"i","resources":{"requests":{"nvidia.com/gpu":2},"limits":{"nvidia.com/gpu":1}}}]}}'
P3_G92_CODE="$(curl -s -o "$P3_TMP/g92.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G92" "$BASE$P3_PODS" 2>/dev/null || true)"
P3_G92_BODY="$(cat "$P3_TMP/g92.json" 2>/dev/null || true)"
if [[ "$P3_G92_CODE" == "422" ]] && printf '%s' "$P3_G92_BODY" | grep -qF 'must be less than or equal to nvidia.com/gpu limit of 1'; then
  ok "G92  POST pods GPU requests 2 > limits 1 -> 422, message names the resource and limit"
  PASS=$((PASS+1))
else
  echo "FAIL G92  GPU request>limit probe (code=$P3_G92_CODE body=$P3_G92_BODY; expected 422 + less-than-or-equal message)"
  FAIL=$((FAIL+1))
fi

# --- G93: GPU template without host-direct annotation -> 422 (Q61) ---------
# Both probes request a GPU: one with NO annotations, one with a non-literal
# rootfs value ("host" -- only the literal "/" selects host-direct, Q60).
P3_G93A='{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"g93-gpu-nohd","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"g93"}},"template":{"metadata":{"labels":{"app":"g93"}},"spec":{"containers":[{"name":"c","image":"i","resources":{"requests":{"nvidia.com/gpu":1}}}]}}}}'
P3_G93B='{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"g93-gpu-host","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"g93"}},"template":{"metadata":{"labels":{"app":"g93"},"annotations":{"init-pro.io/rootfs":"host"}},"spec":{"containers":[{"name":"c","image":"i","resources":{"limits":{"nvidia.com/gpu":1}}}]}}}}'
P3_G93A_CODE="$(curl -s -o "$P3_TMP/g93a.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G93A" "$BASE$P3_DEPS" 2>/dev/null || true)"
P3_G93B_CODE="$(curl -s -o "$P3_TMP/g93b.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G93B" "$BASE$P3_DEPS" 2>/dev/null || true)"
P3_G93A_BODY="$(cat "$P3_TMP/g93a.json" 2>/dev/null || true)"
if [[ "$P3_G93A_CODE" == "422" && "$P3_G93B_CODE" == "422" ]] && printf '%s' "$P3_G93A_BODY" | grep -qF 'GPU requests (nvidia.com/gpu) require host-direct'; then
  ok "G93  POST deployments GPU template without (and with non-literal) rootfs annotation -> 422 both (GPU pods are host-direct-only, Q61)"
  PASS=$((PASS+1))
else
  echo "FAIL G93  GPU-without-host-direct probe (nohd=$P3_G93A_CODE host=$P3_G93B_CODE body=$P3_G93A_BODY; expected 422 + require host-direct)"
  FAIL=$((FAIL+1))
fi

# --- G94: host-direct template must use Recreate (Q60) ---------------------
# Probe 1: strategy absent (upstream default RollingUpdate); probe 2: type
# explicitly RollingUpdate. Both must be rejected.
P3_G94A='{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"g94-hd-default","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"g94"}},"template":{"metadata":{"labels":{"app":"g94"},"annotations":{"init-pro.io/rootfs":"/"}},"spec":{"containers":[{"name":"c","image":"i"}]}}}}'
P3_G94B="$(printf '%s' "$P3_G94A" | sed -e 's/"name":"g94-hd-default"/"name":"g94-hd-roll"/' -e 's#"spec":{"replicas":1#"strategy":{"type":"RollingUpdate"},"spec":{"replicas":1#')"
P3_G94A_CODE="$(curl -s -o "$P3_TMP/g94a.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G94A" "$BASE$P3_DEPS" 2>/dev/null || true)"
P3_G94B_CODE="$(curl -s -o "$P3_TMP/g94b.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G94B" "$BASE$P3_DEPS" 2>/dev/null || true)"
P3_G94A_BODY="$(cat "$P3_TMP/g94a.json" 2>/dev/null || true)"
if [[ "$P3_G94A_CODE" == "422" && "$P3_G94B_CODE" == "422" ]] && printf '%s' "$P3_G94A_BODY" | grep -qF 'requires strategy.type Recreate'; then
  ok "G94  POST deployments host-direct template + RollingUpdate (default and explicit) -> 422 both (Q60)"
  PASS=$((PASS+1))
else
  echo "FAIL G94  host-direct RollingUpdate probe (default=$P3_G94A_CODE explicit=$P3_G94B_CODE body=$P3_G94A_BODY; expected 422 + requires strategy.type Recreate)"
  FAIL=$((FAIL+1))
fi

# --- G95: positive host-direct Deployment + annotation propagation ---------
P3_G95='{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"g95-hd","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"g95-hd"}},"strategy":{"type":"Recreate"},"template":{"metadata":{"labels":{"app":"g95-hd"},"annotations":{"init-pro.io/rootfs":"/","init-pro.io/golden":"carried"}},"spec":{"containers":[{"name":"c","image":"i","resources":{"requests":{"nvidia.com/gpu":1},"limits":{"nvidia.com/gpu":1}}}]}}}}'
P3_G95_CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G95" "$BASE$P3_DEPS" 2>/dev/null || true)"
# Wait for the ReplicaSet reconciler to materialize the pod (name prefix
# g95-hd-; labelSelector is not a server-side filter here, negative.sh G52).
P3_G95_POD=""
for _ in $(seq 1 60); do
  P3_G95_POD="$(curl -s --max-time 5 "$BASE$P3_PODS" 2>/dev/null | python3 -c '
import json,sys
try:
    d = json.load(sys.stdin)
    names = [i["metadata"]["name"] for i in d.get("items", [])
             if str(i.get("metadata",{}).get("name","")).startswith("g95-hd-")]
    print(names[0] if names else "")
except Exception:
    print("")' 2>/dev/null || true)"
  [[ -n "$P3_G95_POD" ]] && break
  sleep 0.25
done
P3_G95_ANN=""
if [[ -n "$P3_G95_POD" ]]; then
  P3_G95_ANN="$(curl -s --max-time 5 "$BASE$P3_PODS/$P3_G95_POD" 2>/dev/null | python3 -c '
import json,sys
try:
    d = json.load(sys.stdin)
    ann = d.get("metadata",{}).get("annotations",{})
    print(ann.get("init-pro.io/rootfs","<absent>") + " " + ann.get("init-pro.io/golden","<absent>"))
except Exception:
    print("PARSE-ERROR")' 2>/dev/null || true)"
fi
if [[ "$P3_G95_CODE" == "201" && -n "$P3_G95_POD" && "$P3_G95_ANN" == "/ carried" ]]; then
  ok "G95  POST deployments host-direct (\"/\" + Recreate + nvidia.com/gpu 1) -> 201; pod $P3_G95_POD carries the template annotations verbatim"
  PASS=$((PASS+1))
else
  echo "FAIL G95  positive host-direct deployment (code=$P3_G95_CODE pod=$P3_G95_POD ann=$P3_G95_ANN; expected 201 + pod with rootfs=/ golden=carried)"
  FAIL=$((FAIL+1))
fi
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE$P3_DEPS/g95-hd" 2>/dev/null || true

# --- G96: template branch + apply/replace seams ----------------------------
# (a) the pull-policy rule also fires inside a Deployment Pod template;
# (b) PUT replace of an existing pod with an invalid policy -> 422;
# (c) SSA apply (apply-patch+yaml) of an invalid policy -> 422.
P3_G96A='{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"g96-tmpl-policy","namespace":"default"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"g96"}},"template":{"metadata":{"labels":{"app":"g96"}},"spec":{"containers":[{"name":"c","image":"i","imagePullPolicy":"WhenIdle"}]}}}}'
P3_G96A_CODE="$(curl -s -o "$P3_TMP/g96a.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G96A" "$BASE$P3_DEPS" 2>/dev/null || true)"
P3_G96A_BODY="$(cat "$P3_TMP/g96a.json" 2>/dev/null || true)"
P3_G96B='{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g96-repl","namespace":"default"},"spec":{"schedulerName":"golden-none","containers":[{"name":"c","image":"i","imagePullPolicy":"IfNotPresent"}]}}'
P3_G96B_CREATE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G96B" "$BASE$P3_PODS" 2>/dev/null || true)"
P3_G96B_BAD="$(printf '%s' "$P3_G96B" | sed 's/"imagePullPolicy":"IfNotPresent"/"imagePullPolicy":"sometimes"/')"
P3_G96B_PUT="$(curl -s -o "$P3_TMP/g96b.json" -w '%{http_code}' --max-time 5 -X PUT -H 'Content-Type: application/json' --data-binary "$P3_G96B_BAD" "$BASE$P3_PODS/g96-repl" 2>/dev/null || true)"
P3_G96B_BODY="$(cat "$P3_TMP/g96b.json" 2>/dev/null || true)"
P3_G96C='{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g96-ssa","namespace":"default"},"spec":{"schedulerName":"golden-none","containers":[{"name":"c","image":"i","imagePullPolicy":"Never-ever"}]}}'
P3_G96C_APPLY="$(curl -s -o "$P3_TMP/g96c.json" -w '%{http_code}' --max-time 5 -X PATCH -H 'Content-Type: application/apply-patch+yaml' --data-binary "$P3_G96C" "$BASE$P3_PODS/g96-ssa?fieldManager=golden-phase3" 2>/dev/null || true)"
P3_G96C_BODY="$(cat "$P3_TMP/g96c.json" 2>/dev/null || true)"
if [[ "$P3_G96A_CODE" == "422" ]] && printf '%s' "$P3_G96A_BODY" | grep -qF 'spec.template.spec/containers[0]/imagePullPolicy: Unsupported value' \
  && [[ "$P3_G96B_CREATE" == "201" && "$P3_G96B_PUT" == "422" ]] && printf '%s' "$P3_G96B_BODY" | grep -qF 'Unsupported value' \
  && [[ "$P3_G96C_APPLY" == "422" ]] && printf '%s' "$P3_G96C_BODY" | grep -qF 'Unsupported value'; then
  ok "G96  bad imagePullPolicy: Deployment template POST, pod PUT replace, pod SSA apply -> 422 all three (uniform admission seam)"
  PASS=$((PASS+1))
else
  echo "FAIL G96  write-seam probe (tmpl=$P3_G96A_CODE create=$P3_G96B_CREATE put=$P3_G96B_PUT apply=$P3_G96C_APPLY; expected 422/201/422/422 with Unsupported value)"
  FAIL=$((FAIL+1))
fi
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE$P3_PODS/g96-repl" 2>/dev/null || true
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE$P3_PODS/g96-ssa" 2>/dev/null || true

# --- G97: explicit vendor visibility env on a GPU container -> 422 (F1) ----
# The pod carries the host-direct annotation so only the env-conflict rule
# can fire: the platform injects CUDA_VISIBLE_DEVICES from the GPU
# allocation ledger, so an explicit user value is Forbidden (Q61).
P3_G97='{"apiVersion":"v1","kind":"Pod","metadata":{"name":"g97-env","namespace":"default","annotations":{"init-pro.io/rootfs":"/"}},"spec":{"schedulerName":"golden-none","containers":[{"name":"c","image":"i","resources":{"requests":{"nvidia.com/gpu":1}},"env":[{"name":"CUDA_VISIBLE_DEVICES","value":"all"}]}]}}'
P3_G97_CODE="$(curl -s -o "$P3_TMP/g97.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G97" "$BASE$P3_PODS" 2>/dev/null || true)"
P3_G97_BODY="$(cat "$P3_TMP/g97.json" 2>/dev/null || true)"
if [[ "$P3_G97_CODE" == "422" ]] && printf '%s' "$P3_G97_BODY" | grep -qF 'env[0]: Forbidden' && printf '%s' "$P3_G97_BODY" | grep -qF 'CUDA_VISIBLE_DEVICES'; then
  ok "G97  POST pods GPU container with explicit CUDA_VISIBLE_DEVICES -> 422 Forbidden (platform-injected per ledger, Q61)"
  PASS=$((PASS+1))
else
  echo "FAIL G97  GPU env-conflict probe (code=$P3_G97_CODE body=$P3_G97_BODY; expected 422 + env[0] Forbidden naming CUDA_VISIBLE_DEVICES)"
  FAIL=$((FAIL+1))
fi
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE$P3_PODS/g97-env" 2>/dev/null || true

# --- G98: ReplicaSet GPU template without the rootfs annotation -> 422 (F2)
# replicas:0 so nothing materializes; the GPU-requires-host-direct rule is
# shared across bare Pods and every apps/v1 template, not Deployment-only.
P3_G98='{"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"g98-rs","namespace":"default"},"spec":{"replicas":0,"selector":{"matchLabels":{"app":"g98"}},"template":{"metadata":{"labels":{"app":"g98"}},"spec":{"containers":[{"name":"c","image":"i","resources":{"requests":{"amd.com/gpu":1}}}]}}}}'
P3_G98_CODE="$(curl -s -o "$P3_TMP/g98.json" -w '%{http_code}' --max-time 5 -X POST -H 'Content-Type: application/json' --data-binary "$P3_G98" "$BASE$P3_RSS" 2>/dev/null || true)"
P3_G98_BODY="$(cat "$P3_TMP/g98.json" 2>/dev/null || true)"
if [[ "$P3_G98_CODE" == "422" ]] && printf '%s' "$P3_G98_BODY" | grep -qF 'GPU requests (amd.com/gpu) require host-direct'; then
  ok "G98  POST replicasets GPU template without rootfs annotation -> 422 (rule now covers all pod templates + bare pods, Q61)"
  PASS=$((PASS+1))
else
  echo "FAIL G98  RS GPU-without-annotation probe (code=$P3_G98_CODE body=$P3_G98_BODY; expected 422 + require host-direct)"
  FAIL=$((FAIL+1))
fi
curl -s -o /dev/null --max-time 5 -X DELETE "$BASE$P3_RSS/g98-rs" 2>/dev/null || true

# --- teardown hygiene: nothing this suite leaves behind ---
rm -rf "$P3_TMP" 2>/dev/null || true
