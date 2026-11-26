#!/usr/bin/env bash
# Layer 8 delivery-plane e2e (TODO **T8.1-T8.5**, decisions Q40/Q56/Q57/Q58):
# a REAL `init-pro server` (embedded registry + ServicePipeline plane, both
# default-on) driven over the REST + Registry V2 HTTP surface with curl and
# python3-json only, with a fake `docker` CLI shim exported to the server
# process alone through INIT_PRO_DOCKER. Fully offline: every byte of the
# image is a locally fabricated plain `docker save` tar (manifest.json +
# cfg.json + layer.tar, the crates/controllers/tests/pipeline.rs fixture).
# The importer (crates/registry/src/tarimg.rs) streams config + layers into
# the registry and publishes under `spec.image.repository:tag` -- the
# archive's RepoTags are informational and need NOT match the spec.
#
#   DL1  boot      — registry `/v2/` 200 + init-pro.io/v1 discovery lists
#                    servicepipelines/builds/trafficsplits (the apiserver in
#                    this build has no /readyz; discovery is the probe).
#   DL2  build     — Deployment web + ServicePipeline web (context mode) ->
#                    ONE Build child `Succeeded` with `fake docker build ok`
#                    in status.logTail; pipeline `Promoted` (T8.3/Q56).
#   DL3  publish   — registry tags/list carries v1 (T8.1 push client, Q40).
#   DL4  promote   — Deployment container `app` image == <reg>/team/web:v1,
#                    status.observedTag == v1.
#   DL5  re-build  — tag v2 -> a SECOND Succeeded Build, observedTag v2,
#                    Deployment image :v2, tags v1+v2 (Q56: one Build/tag).
#   DL6  split     — Services web-entry/web-stable + TrafficSplit web-entry
#                    round-trips its spec (T8.2/Q57).
#   DL7  canary    — spec.release steps 20%/50% -> canary Deployment
#                    `web-canary` (<target.deployment>-canary) + Service
#                    `web-stable-canary` (<stableService>-canary) -> Promoted,
#                    weights stable 100 / canary 0, stable image :v3
#                    (T8.5/Q58).
#   DL8  abort     — tag v4 soaking at 30% + abort=true -> Aborted, children
#                    gone, observedTag/image stay v3, weights 100/0.
#   DL9  invalid   — weight 150 -> Failed + Ready=False/ReleaseInvalid.
#   DL10 archive   — source.archive (docker-free, Q56) + restart with a
#                    nonexistent docker bin -> Build Succeeded, Promoted v6,
#                    registry content survived the restart (data dir, Q40).
#
# SKIP contract: missing binary/curl/python3/tar or unwired flags => one
# `SKIP ` line on stdout, exit 0. Each check prints `OK   DL<n> <text>`;
# failures print `FAIL DL<n> <text>` to stderr and are counted; the last line
# is `PASS delivery-e2e (N checks)` or `FAIL delivery-e2e (...)` (exit 1).
# Child processes die through trap EXIT; no kubelet/agent is involved (the
# Deployment's pods are never scheduled onto anything).
#
# Usage: scripts/delivery-e2e.sh
#   INIT_PRO_BIN=...  reuse a pre-built binary (default: target/debug/init-pro)
#   KEEP_TMP=1        keep the temp dir (server logs, fixture, docker shim)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="${INIT_PRO_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/debug/init-pro}"
KEEP_TMP="${KEEP_TMP:-0}"
NS="default"; GROUP="init-pro.io/v1"
NSP="/apis/$GROUP/namespaces/$NS"            # init-pro.io CRD namespace path
DEPP="/apis/apps/v1/namespaces/$NS"          # apps/v1 namespace path
SVCP="/api/v1/namespaces/$NS"                # core/v1 namespace path
REPO="team/web"                              # repository the pipeline pushes
TS_NAME="web-entry"                          # TrafficSplit == entry Service (Q57)
CAN_DEP="web-canary"                         # <target.deployment>-canary
CAN_SVC="web-stable-canary"                  # <stableService>-canary

PASS=0; FAIL=0; WHY=""; NOTE=""; SRV_PID=""; SRV_LOG=""
ok()   { echo "OK   $*"; PASS=$((PASS+1)); }
bad()  { echo "FAIL $*" >&2; FAIL=$((FAIL+1)); }
skip() { echo "SKIP $*"; }
skip_quit() { echo "SKIP delivery-e2e: $*"; exit 0; }
summary() {
  if (( FAIL > 0 )); then
    echo "FAIL delivery-e2e ($PASS passed, $FAIL failed)"; exit 1
  fi
  echo "PASS delivery-e2e ($PASS checks)"
}

# --- preflight (SKIP contract: exit 0, one SKIP line) ----------------------
[[ -x "$BIN" ]] || skip_quit "no binary at $BIN (run 'cargo build --workspace --locked' or set INIT_PRO_BIN)"
command -v curl    >/dev/null 2>&1 || skip_quit "curl is required (JSON over HTTP)"
command -v python3 >/dev/null 2>&1 || skip_quit "python3 is required (JSON decoding)"
command -v tar     >/dev/null 2>&1 || skip_quit "tar is required (docker save fixture)"
SERVER_HELP="$("$BIN" server --help 2>&1 || true)"   # snapshot once; no grep -q SIGPIPE under pipefail
for f in --registry-addr --pipeline-dir --disable-pipeline; do
  grep -q -- "$f" <<<"$SERVER_HELP" || skip_quit "\`server --help\` lacks $f (delivery plane not wired)"
done

# --- helpers ---------------------------------------------------------------
pick_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'; }
stop_pid() {
  local p="$1" i
  [[ -n "$p" ]] || return 0
  kill -TERM "$p" 2>/dev/null || true
  for i in $(seq 1 50); do kill -0 "$p" 2>/dev/null || return 0; sleep 0.1; done
  kill -KILL "$p" 2>/dev/null || true
  wait "$p" 2>/dev/null || true
}
dump_log() {
  echo "----- $SRV_LOG (tail) -----" >&2
  tail -30 "$SRV_LOG" >&2 2>/dev/null || true
}
run_gate() { # ID DESC FN -> per-gate ok/bad line; FN sets WHY on failure, NOTE on success
  local id="$1" desc="$2" fn="$3"
  WHY=""; NOTE=""
  if "$fn"; then ok "$id  $desc${NOTE:+ [$NOTE]}"; else bad "$id  $desc (${WHY:-failed})"; dump_log; fi
}
# jget FILE [EXPR [ARG]] -> EXPR evaluated over the parsed document `d`
#   (strings raw, other values as JSON). `x` inside EXPR is ARG.
jget()  { python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); x=sys.argv[3] if len(sys.argv)>3 else None
v=eval(sys.argv[2]); print(v if isinstance(v,str) else json.dumps(v))' "$@" 2>/dev/null || true; }
# jtest FILE [EXPR [ARG]] -> exit 0 when EXPR is truthy (missing keys / bad JSON fail)
jtest() { python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); x=sys.argv[3] if len(sys.argv)>3 else None
sys.exit(0 if eval(sys.argv[2]) else 1)' "$@" 2>/dev/null; }
http() { # METHOD FILE PATH -> status code (RFC 7386 merge-patch for PATCH: null deletes)
  local ct="application/json"
  if [[ "$1" == PATCH ]]; then ct="application/merge-patch+json"; fi
  curl -s -o "$TMP/last.out" -w '%{http_code}' -X "$1" -H "Content-Type: $ct" \
    --data-binary "@$2" "$BASE$3" 2>/dev/null || true
}
code() { curl -s -o /dev/null -w '%{http_code}' "$1" 2>/dev/null || true; }          # full URL
get()  { : > "$2"; curl -s -o "$2" -w '%{http_code}' "$BASE$1" 2>/dev/null || true; } # PATH OUT
wait_until() { # SECONDS FN [ARGS...] -> 0 once FN succeeds; 1 s poll, bounded
  local deadline=$((SECONDS + $1)); shift
  while (( SECONDS <= deadline )); do
    "$@" && return 0
    kill -0 "$SRV_PID" 2>/dev/null || { WHY="server exited during the wait"; return 1; }
    sleep 1
  done
  return 1
}
# --- object readers --------------------------------------------------------
sp_get()        { get "$NSP/servicepipelines/web" "$OUT/sp.json" >/dev/null; }
builds_get()    { get "$NSP/builds" "$OUT/builds.json" >/dev/null; }
ts_get()        { get "$NSP/trafficsplits/$TS_NAME" "$OUT/ts.json" >/dev/null; }
dep_get()       { get "$DEPP/deployments/$1" "$OUT/dep.json" >/dev/null; }
tags_get()      { curl -s "$REG/v2/$REPO/tags/list" > "$OUT/tags.json" 2>/dev/null || true; }
sp_phase()      { sp_get; jget "$OUT/sp.json" "d.get('status',{}).get('phase')"; }
sp_phase_in()   { sp_phase >/dev/null; jtest "$OUT/sp.json" "d.get('status',{}).get('phase') in [$1]"; }
sp_observes()   { sp_get; jtest "$OUT/sp.json" "d['status'].get('observedTag') == '$1' and d['status'].get('phase') in [$2]"; }
observed_tag()  { sp_get; jget "$OUT/sp.json" "d['status'].get('observedTag')"; }
stable_image()  { dep_get web; jget "$OUT/dep.json" "d['spec']['template']['spec']['containers'][0]['image']"; }
canary_image()  { dep_get "$CAN_DEP"; jget "$OUT/dep.json" "d['spec']['template']['spec']['containers'][0]['image']"; }
weights()       { ts_get; jget "$OUT/ts.json" "' '.join(b['service']+'='+str(b['weight']) for b in d['spec']['backends'])"; }
weights_are()   { ts_get; jtest "$OUT/ts.json" "[(b['service'],b['weight']) for b in d['spec']['backends']] == [('web-stable', $1), ('$CAN_SVC', $2)]"; }
children_code() { echo "$(code "$BASE$DEPP/deployments/$CAN_DEP")/$(code "$BASE$SVCP/services/$CAN_SVC")"; }
children_exist() { [[ "$(children_code)" == "200/200" ]]; }
children_gone()  { [[ "$(children_code)" == "404/404" ]]; }
succeeded_builds() { builds_get; jget "$OUT/builds.json" "sum(1 for b in d['items'] if b.get('status',{}).get('phase')=='Succeeded')"; }
builds_summary()   { builds_get; jget "$OUT/builds.json" "[(b['metadata']['name'], b.get('status',{}).get('phase')) for b in d['items']]"; }
v4_soaking()     { sp_get; jtest "$OUT/sp.json" "d['status'].get('rollout',{}).get('revision') == x" "$REF:v4" && children_exist; }
# settled TAG -> terminal verdict for TAG (observedTag advanced), OR the tag's
# Build already Failed. The second clause is needed because a failed Build
# leaves the pipeline at `Building` (hold_failure's already-reported guard sees
# the lastBuild name create_build stamped) -- report that quickly, not at 90 s.
settled() {
  sp_observes "$1" "'Promoted','Failed'" && return 0
  builds_get
  jtest "$OUT/builds.json" "any(b['spec']['image']['tag'] == '$1' and b.get('status',{}).get('phase') == 'Failed' for b in d['items'])"
}

# --- setup -----------------------------------------------------------------
TMP="$(mktemp -d /tmp/delivery-e2e.XXXXXX)"
DATA="$TMP/server-data"; OUT="$TMP/out"; mkdir -p "$OUT"
LOG1="$TMP/server-1.log"; LOG2="$TMP/server-2.log"
FIXDIR="$TMP/fixture"; CTX="$TMP/ctx"; FIXTURE="$TMP/fixture.tar"
SHIM="$TMP/docker-shim"; CALLS="$TMP/docker.calls"; : > "$CALLS"
unset INIT_PRO_DISABLE_REGISTRY INIT_PRO_DISABLE_ANYHOME INIT_PRO_REGISTRY_ADDR \
  INIT_PRO_REGISTRY_TAG_RETENTION INIT_PRO_DISABLE_PIPELINE INIT_PRO_PIPELINE_DIR INIT_PRO_DOCKER
RPORT="$(pick_port)"; APORT="$(pick_port)"
while [[ "$APORT" == "$RPORT" ]]; do APORT="$(pick_port)"; done
BASE="http://127.0.0.1:$APORT"; REG="http://127.0.0.1:$RPORT"; REF="$REG/$REPO"

cleanup() {
  trap - EXIT INT TERM
  stop_pid "$SRV_PID"
  cat "$LOG1" "$LOG2" 2>/dev/null > /tmp/init-pro-delivery.log || true
  if [[ "$KEEP_TMP" == "1" ]]; then echo "delivery-e2e: artifacts kept at $TMP (KEEP_TMP=1)"; return 0; fi
  rm -rf "$TMP" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# docker save fixture (plain uncompressed tar, the pipeline.rs layout) + ctx
mkdir -p "$FIXDIR" "$CTX"
printf '%s' '[{"Config":"cfg.json","RepoTags":["team/web:v1"],"Layers":["layer.tar"]}]' > "$FIXDIR/manifest.json"
printf '%s' '{"architecture":"amd64","os":"linux"}' > "$FIXDIR/cfg.json"
printf 'layer bytes for the delivery e2e\n' > "$FIXDIR/layer.tar"
tar -C "$FIXDIR" -cf "$FIXTURE" manifest.json cfg.json layer.tar
printf 'FROM scratch\n' > "$CTX/Dockerfile"
# Fake docker: build ok, `save -o <tar> <ref>` copies the fixture to $3; the
# B8 hygiene calls (images/rmi) are answered empty so nothing is "pruned".
cat > "$SHIM" <<SHIM_EOF
#!/bin/sh
echo "\$*" >> "$CALLS"
case "\$1" in
  build) echo "fake docker build ok"; exit 0;;
  save)  cp "$FIXTURE" "\$3"; echo "fake docker save ok"; exit 0;;
  images) exit 0;;
  rmi)   exit 0;;
  *)     echo "unexpected fake docker args: \$*" >&2; exit 2;;
esac
SHIM_EOF
chmod +x "$SHIM"

sp_json() { # TAG SOURCE-JSON -> ServicePipeline web body
  printf '{"apiVersion":"init-pro.io/v1","kind":"ServicePipeline","metadata":{"name":"web","namespace":"%s"},"spec":{"image":{"repository":"%s","tag":"%s"},"source":%s,"build":{"timeoutSeconds":30},"target":{"deployment":"web","container":"app"}}}' \
    "$NS" "$REF" "$1" "$2"
}
cat > "$TMP/dep.json" <<JSON_EOF
{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web","namespace":"$NS"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"web"}},"template":{"metadata":{"labels":{"app":"web"}},"spec":{"containers":[{"name":"app","image":"nginx:alpine"}]}}}}
JSON_EOF
sp_json v1 "{\"context\":\"$CTX\",\"dockerfile\":\"Dockerfile\"}" > "$TMP/sp-v1.json"
sp_json v6 "{\"archive\":\"$FIXTURE\"}" > "$TMP/sp-v6.json"
printf '%s' '{"spec":{"image":{"tag":"v2"}}}' > "$TMP/tag-v2.json"
cat > "$TMP/svc-entry.json" <<JSON_EOF
{"apiVersion":"v1","kind":"Service","metadata":{"name":"web-entry","namespace":"$NS"},"spec":{"type":"NodePort","selector":{"app":"web"},"ports":[{"name":"http","port":80,"targetPort":8080,"protocol":"TCP"}]}}
JSON_EOF
cat > "$TMP/svc-stable.json" <<JSON_EOF
{"apiVersion":"v1","kind":"Service","metadata":{"name":"web-stable","namespace":"$NS"},"spec":{"type":"ClusterIP","selector":{"app":"web"},"ports":[{"name":"http","port":80,"targetPort":8080,"protocol":"TCP"}]}}
JSON_EOF
cat > "$TMP/ts.json" <<JSON_EOF
{"apiVersion":"init-pro.io/v1","kind":"TrafficSplit","metadata":{"name":"$TS_NAME","namespace":"$NS"},"spec":{"service":"$TS_NAME","backends":[{"service":"web-stable","weight":100}]}}
JSON_EOF
REL_BASE="\"trafficSplit\":\"$TS_NAME\",\"stableService\":\"web-stable\""
printf '%s' "{\"spec\":{\"image\":{\"tag\":\"v3\"},\"release\":{$REL_BASE,\"steps\":[{\"weight\":20,\"pauseSeconds\":1},{\"weight\":50,\"pauseSeconds\":1}]}}}" > "$TMP/rel-v3.json"
printf '%s' "{\"spec\":{\"image\":{\"tag\":\"v4\"},\"release\":{$REL_BASE,\"abort\":false,\"steps\":[{\"weight\":30,\"pauseSeconds\":60}]}}}" > "$TMP/rel-v4.json"
printf '%s' '{"spec":{"release":{"abort":true}}}' > "$TMP/abort.json"
printf '%s' "{\"spec\":{\"image\":{\"tag\":\"v5\"},\"release\":{$REL_BASE,\"abort\":false,\"steps\":[{\"weight\":150,\"pauseSeconds\":1}]}}}" > "$TMP/rel-v5.json"
printf '%s' "{\"spec\":{\"image\":{\"tag\":\"v6\"},\"release\":null,\"source\":{\"context\":null,\"archive\":\"$FIXTURE\"}}}" > "$TMP/rel-v6.json"

boot() { # DOCKER_BIN LOG -> server with registry + pipeline planes default-on
  SRV_LOG="$2"
  INIT_PRO_DOCKER="$1" "$BIN" server --data-dir "$DATA" --token e2e-delivery-token \
    --bind-address 127.0.0.1 --https-listen-port "$APORT" --disable-anyhome \
    --registry-addr "127.0.0.1:$RPORT" --registry-tag-retention 10 >"$SRV_LOG" 2>&1 &
  SRV_PID=$!
  local i
  for i in $(seq 1 150); do   # ~30 s budget for the apiserver
    curl -sf -o /dev/null "$BASE/api/v1" 2>/dev/null && break
    kill -0 "$SRV_PID" 2>/dev/null || { WHY="server exited during boot"; return 1; }
    sleep 0.2
  done
  for i in $(seq 1 50); do    # ~10 s budget for the registry V2 ping
    curl -sf -o /dev/null "$REG/v2/" 2>/dev/null && return 0
    kill -0 "$SRV_PID" 2>/dev/null || { WHY="server exited before /v2/ answered"; return 1; }
    sleep 0.2
  done
  WHY="registry /v2/ never answered"; return 1
}
restart_server() { stop_pid "$SRV_PID"; SRV_PID=""; boot "$1" "$2"; }

# --- DL1: boot -------------------------------------------------------------
gate_dl1() {
  boot "$SHIM" "$LOG1" || return 1
  [[ "$(get "/apis/$GROUP" "$OUT/apis.json")" == 200 ]] || { WHY="GET /apis/$GROUP != 200"; return 1; }
  for r in servicepipelines builds trafficsplits; do
    jtest "$OUT/apis.json" "'$r' in [x['name'] for x in d['resources']]" || { WHY="discovery lacks $r"; return 1; }
  done
  [[ "$(code "$REG/v2/")" == 200 ]] || { WHY="GET $REG/v2/ != 200"; return 1; }
  [[ -x "$SHIM" ]] || { WHY="docker shim $SHIM missing"; return 1; }
}
# --- DL2: pipeline -> Build -> fake docker -> Promoted ---------------------
gate_dl2() {
  local c
  c="$(http POST "$TMP/dep.json" "$DEPP/deployments")"; [[ "$c" == 201 ]] || { WHY="Deployment POST -> $c"; return 1; }
  c="$(http POST "$TMP/sp-v1.json" "$NSP/servicepipelines")"; [[ "$c" == 201 ]] || { WHY="ServicePipeline POST -> $c: $(head -c 200 "$TMP/last.out")"; return 1; }
  wait_until 90 settled v1 || { WHY="phase='$(sp_phase)' after 90 s, builds=$(builds_summary)"; return 1; }
  [[ "$(sp_phase)" == Promoted ]] || { WHY="phase=$(sp_phase): $(jget "$OUT/sp.json" "d['status'].get('conditions')")"; return 1; }
  builds_get
  jtest "$OUT/builds.json" "len(d['items']) == 1 and d['items'][0]['status']['phase'] == 'Succeeded'" \
    || { WHY="expected exactly one Succeeded Build, got $(builds_summary)"; return 1; }
  jtest "$OUT/builds.json" "'fake docker build ok' in d['items'][0]['status'].get('logTail','')" \
    || { WHY="Build logTail lacks 'fake docker build ok': $(jget "$OUT/builds.json" "d['items'][0]['status'].get('logTail')")"; return 1; }
  NOTE="Build $(jget "$OUT/builds.json" "d['items'][0]['metadata']['name']")"
}
# --- DL3/DL4: registry tag + Deployment patch -------------------------------
gate_dl3() {
  tags_get
  jtest "$OUT/tags.json" "'v1' in d.get('tags', [])" || { WHY="tags/list = $(cat "$OUT/tags.json")"; return 1; }
}
gate_dl4() {
  dep_get web
  jtest "$OUT/dep.json" "d['spec']['template']['spec']['containers'][0]['name'] == 'app' and d['spec']['template']['spec']['containers'][0]['image'] == x" "$REF:v1" \
    || { WHY="Deployment image = $(stable_image) (want $REF:v1)"; return 1; }
  sp_get; jtest "$OUT/sp.json" "d['status'].get('observedTag') == 'v1'" || { WHY="observedTag = $(observed_tag)"; return 1; }
}
# --- DL5: second tag transition -> second immutable Build --------------------
gate_dl5() {
  local c
  c="$(http PATCH "$TMP/tag-v2.json" "$NSP/servicepipelines/web")"; [[ "$c" == 200 ]] || { WHY="PATCH tag v2 -> $c"; return 1; }
  wait_until 90 settled v2 || { WHY="observedTag=$(observed_tag) phase=$(sp_phase) after 90 s, builds=$(builds_summary)"; return 1; }
  [[ "$(sp_phase)" == Promoted ]] || { WHY="phase=$(sp_phase)"; return 1; }
  [[ "$(succeeded_builds)" == 2 ]] || { WHY="Succeeded Builds = $(succeeded_builds): $(builds_summary)"; return 1; }
  [[ "$(stable_image)" == "$REF:v2" ]] || { WHY="Deployment image = $(stable_image)"; return 1; }
  tags_get
  jtest "$OUT/tags.json" "'v1' in d.get('tags', []) and 'v2' in d.get('tags', [])" || { WHY="tags/list = $(cat "$OUT/tags.json")"; return 1; }
}
# --- DL6: TrafficSplit round-trip (Q57) --------------------------------------
gate_dl6() {
  local c
  c="$(http POST "$TMP/svc-entry.json" "$SVCP/services")"; [[ "$c" == 201 ]] || { WHY="Service web-entry POST -> $c"; return 1; }
  c="$(http POST "$TMP/svc-stable.json" "$SVCP/services")"; [[ "$c" == 201 ]] || { WHY="Service web-stable POST -> $c"; return 1; }
  c="$(http POST "$TMP/ts.json" "$NSP/trafficsplits")"; [[ "$c" == 201 ]] || { WHY="TrafficSplit POST -> $c"; return 1; }
  ts_get
  jtest "$OUT/ts.json" "d['spec'] == {'service': '$TS_NAME', 'backends': [{'service': 'web-stable', 'weight': 100}]}" \
    || { WHY="TrafficSplit spec = $(jget "$OUT/ts.json" "d['spec']")"; return 1; }
}
# --- DL7: canary release 20% -> 50% -> promote -------------------------------
gate_dl7() {
  local c seen=" " p w deadline=$((SECONDS + 120))
  c="$(http PATCH "$TMP/rel-v3.json" "$NSP/servicepipelines/web")"; [[ "$c" == 200 ]] || { WHY="PATCH release + tag v3 -> $c"; return 1; }
  while (( SECONDS <= deadline )); do   # 0.5 s poll: catch the soak states if we can
    p="$(sp_phase)"; w="$(weights)"
    [[ "$seen" == *" $p:$w "* ]] || seen="$seen$p:$w "
    [[ "$p" == Promoted || "$p" == Failed ]] && break
    kill -0 "$SRV_PID" 2>/dev/null || { WHY="server exited during the canary roll"; return 1; }
    sleep 0.5
  done
  [[ "$p" == Promoted ]] || { WHY="phase=$p after 120 s (seen:$seen)"; return 1; }
  children_exist || { WHY="canary children after promote: $(children_code)"; return 1; }
  weights_are 100 0 || { WHY="weights after promote: $(weights)"; return 1; }
  [[ "$(canary_image)" == "$REF:v3" ]] || { WHY="canary Deployment image = $(canary_image)"; return 1; }
  [[ "$(stable_image)" == "$REF:v3" ]] || { WHY="stable Deployment image = $(stable_image)"; return 1; }
  [[ "$(observed_tag)" == v3 ]] || { WHY="observedTag = $(observed_tag)"; return 1; }
  NOTE="observed:$seen"
}
# --- DL8: abort during the 30% soak ------------------------------------------
gate_dl8() {
  local c
  c="$(http PATCH "$TMP/rel-v4.json" "$NSP/servicepipelines/web")"; [[ "$c" == 200 ]] || { WHY="PATCH tag v4 (30%, soak 60 s) -> $c"; return 1; }
  wait_until 60 v4_soaking || { WHY="v4 canary never soaked: phase=$(sp_phase) rollout=$(jget "$OUT/sp.json" "d['status'].get('rollout')") children=$(children_code)"; return 1; }
  NOTE="soaked at $(weights) before abort"
  c="$(http PATCH "$TMP/abort.json" "$NSP/servicepipelines/web")"; [[ "$c" == 200 ]] || { WHY="PATCH abort=true -> $c"; return 1; }
  wait_until 90 sp_phase_in "'Aborted'" || { WHY="phase=$(sp_phase) after abort (children $(children_code), weights $(weights))"; return 1; }
  children_gone || { WHY="canary children still present: $(children_code)"; return 1; }
  [[ "$(observed_tag)" == v3 ]] || { WHY="observedTag = $(observed_tag) (want v3)"; return 1; }
  [[ "$(stable_image)" == "$REF:v3" ]] || { WHY="stable Deployment image = $(stable_image) (want :v3)"; return 1; }
  weights_are 100 0 || { WHY="weights after abort: $(weights)"; return 1; }
}
# --- DL9: invalid release plan -> Failed/ReleaseInvalid ----------------------
gate_dl9() {
  local c
  c="$(http PATCH "$TMP/rel-v5.json" "$NSP/servicepipelines/web")"; [[ "$c" == 200 ]] || { WHY="PATCH weight 150 + tag v5 -> $c"; return 1; }
  wait_until 90 sp_phase_in "'Failed'" || { WHY="phase=$(sp_phase) after the invalid release"; return 1; }
  sp_get
  jtest "$OUT/sp.json" "any(c.get('type')=='Ready' and c.get('status')=='False' and c.get('reason')=='ReleaseInvalid' for c in d['status'].get('conditions',[]))" \
    || { WHY="conditions = $(jget "$OUT/sp.json" "d['status'].get('conditions')")"; return 1; }
  NOTE="$(jget "$OUT/sp.json" "d['status']['conditions'][0]['message']")"
}
# --- DL10: archive mode is docker-free; registry outlives the server ---------
gate_dl10() {
  local c before after
  c="$(http PATCH "$TMP/rel-v6.json" "$NSP/servicepipelines/web")"; [[ "$c" == 200 ]] || { WHY="PATCH archive mode -> $c"; return 1; }
  sp_get   # RFC 7386: null removed release + source.context (merge-patch on a CR)
  jtest "$OUT/sp.json" "'release' not in d['spec'] and 'context' not in d['spec']['source'] and d['spec']['source'].get('archive') == x" "$FIXTURE" \
    || { WHY="archive-mode spec not applied: $(jget "$OUT/sp.json" "d['spec']")"; return 1; }
  before="$(wc -l < "$CALLS")"
  # The default embedded store is in-memory (Q17): objects vanish, the
  # registry (data-dir backed, Q40) must not. Re-apply the objects after boot.
  restart_server /nonexistent/docker-fake "$LOG2" || return 1
  tags_get
  jtest "$OUT/tags.json" "'v1' in d.get('tags', []) and 'v5' in d.get('tags', [])" || { WHY="registry content lost across the restart: $(cat "$OUT/tags.json")"; return 1; }
  [[ "$(code "$BASE$NSP/servicepipelines/web")" == 404 ]] || { WHY="ServicePipeline survived an in-memory restart?"; return 1; }
  c="$(http POST "$TMP/dep.json" "$DEPP/deployments")"; [[ "$c" == 201 ]] || { WHY="Deployment re-POST -> $c"; return 1; }
  c="$(http POST "$TMP/sp-v6.json" "$NSP/servicepipelines")"; [[ "$c" == 201 ]] || { WHY="ServicePipeline re-POST -> $c"; return 1; }
  wait_until 90 settled v6 || { WHY="observedTag=$(observed_tag) phase=$(sp_phase) after restart, builds=$(builds_summary)"; return 1; }
  [[ "$(sp_phase)" == Promoted ]] || { WHY="phase=$(sp_phase): $(jget "$OUT/sp.json" "d['status'].get('conditions')")"; return 1; }
  builds_get
  jtest "$OUT/builds.json" "len(d['items']) == 1 and d['items'][0]['status']['phase'] == 'Succeeded' and not d['items'][0]['status'].get('logTail')" \
    || { WHY="archive Build not cleanly Succeeded: $(builds_summary)"; return 1; }
  [[ "$(stable_image)" == "$REF:v6" ]] || { WHY="Deployment image = $(stable_image)"; return 1; }
  tags_get
  jtest "$OUT/tags.json" "'v6' in d.get('tags', []) and 'v1' in d.get('tags', [])" || { WHY="tags/list = $(cat "$OUT/tags.json")"; return 1; }
  after="$(wc -l < "$CALLS")"
  [[ "$after" == "$before" ]] || { WHY="docker shim invoked $((after - before))x in archive mode"; return 1; }
  NOTE="docker bin nonexistent, no docker call; tags after restart: $(jget "$OUT/tags.json" "d.get('tags')")"
}

# --- run the checks ---------------------------------------------------------
run_gate DL1 "server booted with registry (/v2/ 200) + pipeline plane; init-pro.io/v1 discovery lists servicepipelines/builds/trafficsplits; fake docker shim on INIT_PRO_DOCKER" gate_dl1
if (( FAIL > 0 )); then
  for n in 2 3 4 5 6 7 8 9 10; do skip "DL$n  not attempted (DL1 boot failed)"; done
  summary
fi
run_gate DL2 "Deployment web + ServicePipeline web -> one Build child Succeeded (logTail has 'fake docker build ok'), pipeline Promoted" gate_dl2
run_gate DL3 "registry GET /v2/$REPO/tags/list contains v1" gate_dl3
run_gate DL4 "Deployment web container app image == $REPO:v1 at the registry host; status.observedTag == v1" gate_dl4
if (( FAIL > 0 )); then
  for n in 5 6 7 8 9 10; do skip "DL$n  not attempted (DL2-DL4 did not pass)"; done
  summary
fi
run_gate DL5 "tag v2 -> second Succeeded Build, observedTag v2, Deployment image :v2, tags v1+v2" gate_dl5
run_gate DL6 "Services web-entry/web-stable + TrafficSplit web-entry round-trip {service: web-entry, backends: [web-stable 100]}" gate_dl6
run_gate DL7 "release 20%/50% + tag v3 -> canary Deployment $CAN_DEP + Service $CAN_SVC, Promoted, weights stable 100 / canary 0, stable image :v3" gate_dl7
run_gate DL8 "tag v4 soaking at 30% + abort=true -> Aborted, canary children 404, observedTag v3, stable image :v3, weights 100/0" gate_dl8
run_gate DL9 "release weight 150 + tag v5 -> phase Failed with Ready=False/ReleaseInvalid" gate_dl9
run_gate DL10 "source.archive + restart with INIT_PRO_DOCKER=/nonexistent/docker-fake -> Build Succeeded docker-free, Promoted v6, registry tags survived the restart" gate_dl10
summary
