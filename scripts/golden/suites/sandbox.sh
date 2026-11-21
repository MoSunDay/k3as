# T7.7 (Q63) sandbox admission suite: the api-visible half of the hardened
# Agent sandbox — spec.sandbox / spec.policy SHAPE validation in admission
# (crates/api/src/agent_policy.rs validate_agent_spec, wired into
# validate::admit for create/apply/replace) plus the round-trip and the
# preserved legacy Q37 path (absent spec.sandbox stays a plain Agent).
# Node-side containment (nsrun/engine/broker, SA1-SA6) is gated by
# scripts/sandbox-e2e.sh instead. IMPLEMENTED semantics; every check
# asserts INTENDED-vs-ACTUAL via status code + a stable message substring.
# Sourced by golden-conformance.sh; runs in the main script's shell
# (helpers: ok, counters PASS/FAIL, $BASE). Owned by plan item: T7.7 (Q63).
# Check-ID range reserved: G99-G106. Hard cap 400 lines.
#
#   G99  positive: POST hardened Agent (sandbox mode/limits) -> 201 and
#        GET round-trips spec.sandbox verbatim (uid/gid absent = node
#        defaults 1000, admission only rejects explicit 0).
#   G100 mode "soft" -> 422 Invalid, message pins "hardened" as the only
#        supported value.
#   G101 limits: missing limits object and a zero cpuMillis -> 422 both,
#        message pins "bounded limits" (no unlimited CPU/memory/PIDs).
#   G102 explicit uid 0 / gid 0 -> 422, "non-root identity".
#   G103 policy.allowTools with a malformed class ("ssh-exec") -> 422
#        "unsupported tool class" (admission checks shape; narrowing vs
#        the operator baseline is node-side, SA1c).
#   G104 sandbox.binds with a relative source and a ".." target -> 422,
#        "must be an absolute path".
#   G105 legacy: Agent with NO spec.sandbox -> 201 (Q37 path preserved).
#   G106 SSA: apply-create with sandbox -> 201; identical re-apply -> 200;
#        the stored object still carries spec.sandbox.

SBX_AGENTS="/apis/init-pro.io/v1/namespaces/default/agents"
SBX_TMP="$(mktemp -d /tmp/golden-sandbox.XXXXXX)"
SBX_CLEANUP=()

# post CODE_FILE BODY_FILE NAME -> captures code+body of a plain create.
sbx_post() { # name json-var
  curl -s -o "$SBX_TMP/$1.json" -w '%{http_code}' --max-time 5 -X POST \
    -H 'Content-Type: application/json' --data-binary "$2" \
    "$BASE$SBX_AGENTS" 2>/dev/null || true
}

# --- G99: hardened sandbox round-trips through create + read ---------------
SBX_G99='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"g99-hardened","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-sandbox","sandbox":{"mode":"hardened","limits":{"cpuMillis":500,"memoryBytes":268435456,"pidsMax":64}}}}'
SBX_G99_CODE="$(sbx_post g99 "$SBX_G99")"
SBX_G99_GET="$(curl -s --max-time 5 "$BASE$SBX_AGENTS/g99-hardened" 2>/dev/null || true)"
if [[ "$SBX_G99_CODE" == "201" ]] \
  && printf '%s' "$SBX_G99_GET" | grep -qF '"sandbox":{"mode":"hardened"' \
  && printf '%s' "$SBX_G99_GET" | grep -qF '"cpuMillis":500' \
  && printf '%s' "$SBX_G99_GET" | grep -qF '"pidsMax":64'; then
  ok "G99  POST hardened Agent -> 201; GET round-trips spec.sandbox (mode + limits verbatim)"
  PASS=$((PASS+1))
else
  echo "FAIL G99  hardened round-trip (code=$SBX_G99_CODE get=$SBX_G99_GET; expected 201 + sandbox echo)"
  FAIL=$((FAIL+1))
fi
SBX_CLEANUP+=("$SBX_AGENTS/g99-hardened")

# --- G100: mode "soft" -> 422 (hardened is the only mode) -------------------
SBX_G100='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"g100-soft","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-sandbox","sandbox":{"mode":"soft","limits":{"cpuMillis":500,"memoryBytes":268435456,"pidsMax":64}}}}'
SBX_G100_CODE="$(sbx_post g100 "$SBX_G100")"
SBX_G100_BODY="$(cat "$SBX_TMP/g100.json" 2>/dev/null || true)"
if [[ "$SBX_G100_CODE" == "422" ]] && printf '%s' "$SBX_G100_BODY" | grep -qF 'supported value is' && printf '%s' "$SBX_G100_BODY" | grep -qF 'hardened'; then
  ok "G100  sandbox mode \"soft\" -> 422 Invalid, message pins \"hardened\" as the only value"
  PASS=$((PASS+1))
else
  echo "FAIL G100  soft-mode probe (code=$SBX_G100_CODE body=$SBX_G100_BODY; expected 422 + supported-value message)"
  FAIL=$((FAIL+1))
fi

# --- G101: limits must exist and be positive (no unlimited sandbox) ---------
SBX_G101A='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"g101-nolimits","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-sandbox","sandbox":{"mode":"hardened"}}}'
SBX_G101B='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"g101-zero","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-sandbox","sandbox":{"mode":"hardened","limits":{"cpuMillis":0,"memoryBytes":268435456,"pidsMax":64}}}}'
SBX_G101A_CODE="$(sbx_post g101a "$SBX_G101A")"
SBX_G101B_CODE="$(sbx_post g101b "$SBX_G101B")"
SBX_G101A_BODY="$(cat "$SBX_TMP/g101a.json" 2>/dev/null || true)"
if [[ "$SBX_G101A_CODE" == "422" && "$SBX_G101B_CODE" == "422" ]] \
  && printf '%s' "$SBX_G101A_BODY" | grep -qF 'bounded limits'; then
  ok "G101  missing limits and zero cpuMillis -> 422 both, message pins bounded limits (Q63)"
  PASS=$((PASS+1))
else
  echo "FAIL G101  limits probes (missing=$SBX_G101A_CODE zero=$SBX_G101B_CODE body=$SBX_G101A_BODY; expected 422 + bounded-limits message)"
  FAIL=$((FAIL+1))
fi

# --- G102: explicit uid 0 / gid 0 -> 422 (non-root identity) ----------------
SBX_G102A='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"g102-uid0","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-sandbox","sandbox":{"mode":"hardened","uid":0,"limits":{"cpuMillis":500,"memoryBytes":268435456,"pidsMax":64}}}}'
SBX_G102B='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"g102-gid0","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-sandbox","sandbox":{"mode":"hardened","gid":0,"limits":{"cpuMillis":500,"memoryBytes":268435456,"pidsMax":64}}}}'
SBX_G102A_CODE="$(sbx_post g102a "$SBX_G102A")"
SBX_G102B_CODE="$(sbx_post g102b "$SBX_G102B")"
SBX_G102B_BODY="$(cat "$SBX_TMP/g102b.json" 2>/dev/null || true)"
if [[ "$SBX_G102A_CODE" == "422" && "$SBX_G102B_CODE" == "422" ]] \
  && printf '%s' "$SBX_G102B_BODY" | grep -qF 'non-root identity'; then
  ok "G102  explicit uid 0 and gid 0 -> 422 both, hardened requires a non-root identity"
  PASS=$((PASS+1))
else
  echo "FAIL G102  root-identity probes (uid=$SBX_G102A_CODE gid=$SBX_G102B_CODE body=$SBX_G102B_BODY; expected 422 + non-root message)"
  FAIL=$((FAIL+1))
fi

# --- G103: malformed allowTools class -> 422 --------------------------------
SBX_G103='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"g103-tools","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-sandbox","sandbox":{"mode":"hardened","limits":{"cpuMillis":500,"memoryBytes":268435456,"pidsMax":64}},"policy":{"allowTools":["ssh-exec"]}}}'
SBX_G103_CODE="$(sbx_post g103 "$SBX_G103")"
SBX_G103_BODY="$(cat "$SBX_TMP/g103.json" 2>/dev/null || true)"
if [[ "$SBX_G103_CODE" == "422" ]] && printf '%s' "$SBX_G103_BODY" | grep -qF 'unsupported tool class'; then
  ok "G103  policy.allowTools [\"ssh-exec\"] -> 422, unsupported tool class (shape check)"
  PASS=$((PASS+1))
else
  echo "FAIL G103  allowTools probe (code=$SBX_G103_CODE body=$SBX_G103_BODY; expected 422 + unsupported tool class)"
  FAIL=$((FAIL+1))
fi

# --- G104: non-absolute / ".." binds -> 422 ---------------------------------
SBX_G104='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"g104-binds","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-sandbox","sandbox":{"mode":"hardened","limits":{"cpuMillis":500,"memoryBytes":268435456,"pidsMax":64},"binds":[{"source":"rel/path","target":"/data"},{"source":"/host","target":"/../escape"}]}}}'
SBX_G104_CODE="$(sbx_post g104 "$SBX_G104")"
SBX_G104_BODY="$(cat "$SBX_TMP/g104.json" 2>/dev/null || true)"
if [[ "$SBX_G104_CODE" == "422" ]] && printf '%s' "$SBX_G104_BODY" | grep -qF 'must be an absolute path'; then
  ok "G104  sandbox.binds relative source and \"..\" target -> 422, absolute-path rule"
  PASS=$((PASS+1))
else
  echo "FAIL G104  binds probe (code=$SBX_G104_CODE body=$SBX_G104_BODY; expected 422 + absolute-path message)"
  FAIL=$((FAIL+1))
fi

# --- G105: legacy Agent (no spec.sandbox) -> 201 (Q37 path preserved) -------
SBX_G105='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"g105-legacy","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-sandbox"}}'
SBX_G105_CODE="$(sbx_post g105 "$SBX_G105")"
if [[ "$SBX_G105_CODE" == "201" ]]; then
  ok "G105  Agent without spec.sandbox -> 201, legacy Q37 path preserved"
  PASS=$((PASS+1))
else
  echo "FAIL G105  legacy probe (code=$SBX_G105_CODE; expected 201)"
  FAIL=$((FAIL+1))
fi
SBX_CLEANUP+=("$SBX_AGENTS/g105-legacy")

# --- G106: SSA apply-create 201 + re-apply 200 keeps spec.sandbox -----------
SBX_G106='{"apiVersion":"init-pro.io/v1","kind":"Agent","metadata":{"name":"g106-ssa","namespace":"default"},"spec":{"agent":"opencoder","prompt":"golden-sandbox","sandbox":{"mode":"hardened","limits":{"cpuMillis":250,"memoryBytes":134217728,"pidsMax":32}}}}'
printf '%s' "$SBX_G106" > "$SBX_TMP/g106.json"
SBX_G106_A="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X PATCH \
  -H 'Content-Type: application/apply-patch+yaml' --data-binary "@$SBX_TMP/g106.json" \
  "$BASE$SBX_AGENTS/g106-ssa?fieldManager=golden-sandbox" 2>/dev/null || true)"
SBX_G106_B="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X PATCH \
  -H 'Content-Type: application/apply-patch+yaml' --data-binary "@$SBX_TMP/g106.json" \
  "$BASE$SBX_AGENTS/g106-ssa?fieldManager=golden-sandbox" 2>/dev/null || true)"
SBX_G106_GET="$(curl -s --max-time 5 "$BASE$SBX_AGENTS/g106-ssa" 2>/dev/null || true)"
if [[ "$SBX_G106_A" == "201" && "$SBX_G106_B" == "200" ]] \
  && printf '%s' "$SBX_G106_GET" | grep -qF '"mode":"hardened"' \
  && printf '%s' "$SBX_G106_GET" | grep -qF '"cpuMillis":250'; then
  ok "G106  SSA apply-create 201 + re-apply 200; the stored Agent keeps spec.sandbox"
  PASS=$((PASS+1))
else
  echo "FAIL G106  SSA sweep (apply=$SBX_G106_A reapply=$SBX_G106_B get=$SBX_G106_GET; expected 201/200 + sandbox kept)"
  FAIL=$((FAIL+1))
fi
SBX_CLEANUP+=("$SBX_AGENTS/g106-ssa")

# --- teardown hygiene: nothing this suite leaves behind ---------------------
for p in "${SBX_CLEANUP[@]}"; do
  curl -s -o /dev/null --max-time 5 -X DELETE "$BASE$p" 2>/dev/null || true
done
rm -rf "$SBX_TMP" 2>/dev/null || true
