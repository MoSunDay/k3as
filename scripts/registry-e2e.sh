#!/usr/bin/env bash
# T6.3 (Q40): end-to-end probe of the embedded Docker Registry v2 image
# plane hosted by `init-pro server`. A real server is booted on a mktemp
# data dir and driven over the V2 HTTP surface with curl only — FULLY
# OFFLINE (registry.k8s.io is unreachable in this environment, so every
# byte of the image is fabricated locally with tar/gzip/sha256sum):
#
#   R0  preflight   — `server --help` exposes the registry flags; if not, the
#                     WHOLE run SKIPs with exit 0 so CI stays green while the
#                     CLI wiring lands ("registry flags not wired yet").
#   R1  boot + ping — `server --data-dir <tmp> --registry-addr 127.0.0.1:<p>
#                     --registry-tag-retention 10` answers `GET /v2/` with
#                     200 + `Docker-Distribution-API-Version: registry/2.0`,
#                     and materializes the `<data-dir>/registry/` layout.
#   R2  push        — fabricated docker-v2 image (deterministic `gzip -n`
#                     layer + config JSON, rootfs.diff_ids = UNCOMPRESSED layer
#                     digest) uploaded monolithically: `POST
#                     blobs/uploads/?digest=<d>` x2 -> 201, `PUT manifests/v1`.
#   R3  pull        — `GET manifests/v1`: 200, Docker-Content-Digest ==
#                     sha256sum(pushed bytes), GET by digest byte-identical.
#   R4  retention   — 12 tags pushed => `tags/list` holds exactly the newest
#                     10 (v3..v12); v1/v2 pruned.
#   R5  keep.json   — `<data-dir>/registry/keep.json` = ["e2e/demo:pinned*"]
#                     pins `pinned-old`: it survives 10 further rollovers and
#                     does NOT count toward the 10 (total 11 = pinned-old +
#                     newest 10).
#   R6  delete      — DELETE unknown/pruned digest -> 404 MANIFEST_UNKNOWN;
#                     DELETE by tag -> 400; DELETE live digest -> 202 and its
#                     tag goes too.
#   R7  pull leg    — best-effort REAL pull through the node CRI stack
#                     (`crictl pull 127.0.0.1:<port>/e2e/demo:v3`). Env-gated:
#                     SKIPs unless INIT_PRO_E2E_CONTAINERD=1 AND crictl is on
#                     PATH with a live container runtime endpoint. Minimal
#                     boot of that stack: scripts/agent-exec-e2e.sh.
#
# Doctrine mirrors scripts/durability-e2e.sh (same boot / wait / trap shape)
# and scripts/agent-exec-e2e.sh (ok/bad/skip counters; SKIP never fails).
#
# Usage: scripts/registry-e2e.sh
#   INIT_PRO_BIN=...           reuse a pre-built binary (default target/debug/init-pro)
#   INIT_PRO_E2E_CONTAINERD=1  enable the R7 crictl leg (needs a live runtime endpoint)
#   KEEP_TMP=1                 keep the mktemp work dir for post-mortem
#
# Requires a PRE-BUILT binary (`cargo build --workspace --locked`). No vendor
# bundle, no root, no agent — only the opt-in R7 leg wants containerd.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl >/dev/null || { echo "error: curl is required" >&2; exit 1; }
for c in tar gzip sha256sum; do
  command -v "$c" >/dev/null || { echo "error: $c is required (offline image fabrication)" >&2; exit 1; }
done

PASS=0; FAIL=0; SKIP=0
ok()   { echo "OK   $*"; PASS=$((PASS+1)); }
bad()  { echo "FAIL $*" >&2; FAIL=$((FAIL+1)); }
skip() { echo "SKIP $*"; SKIP=$((SKIP+1)); }
summary() { echo; echo "registry-e2e: $PASS passed, $FAIL failed, $SKIP skipped"; }

# --- R0 preflight: are the registry flags wired on the binary? -------------
# Snapshot --help ONCE (no pipeline into `grep -q`: under `pipefail` an early
# grep exit can SIGPIPE clap and report a false negative).
SERVER_HELP="$("$BIN" server --help 2>&1 || true)"
MISSING_FLAGS=()
for f in --registry-addr --registry-tag-retention; do
  grep -q -- "$f" <<<"$SERVER_HELP" || MISSING_FLAGS+=("$f")
done
if (( ${#MISSING_FLAGS[@]} > 0 )); then
  echo "registry-e2e: binary does not expose ${MISSING_FLAGS[*]} yet" >&2
  skip "R0  registry flags not wired yet on \`server\` (missing: ${MISSING_FLAGS[*]})"
  for g in R1 R2 R3 R4 R5 R6 R7; do skip "$g  not attempted (registry flags not wired yet)"; done
  summary
  echo "registry-e2e: nothing exercised — the CLI wiring (--registry-addr) has not landed in $BIN"
  exit 0
fi
ok "R0  server --help exposes --registry-addr / --registry-tag-retention"

# --- helpers ---------------------------------------------------------------
pick_port() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
      || echo $((20000 + RANDOM % 10000))
  else
    echo $((20000 + RANDOM % 10000))
  fi
}

stop_pid() {
  local p="$1" i
  [[ -n "$p" ]] || return 0
  kill -TERM "$p" 2>/dev/null || true
  for i in $(seq 1 50); do
    kill -0 "$p" 2>/dev/null || return 0
    sleep 0.1
  done
  kill -KILL "$p" 2>/dev/null || true
  wait "$p" 2>/dev/null || true
}

sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
size_of()   { wc -c < "$1" | tr -d ' '; }
digest_of() { printf 'sha256:%s' "$(sha256_of "$1")"; }

# --- setup -----------------------------------------------------------------
TMP="$(mktemp -d /tmp/registry-e2e.XXXXXX)"
DATA="$TMP/server-data"
SRV_LOG="$TMP/server.log"
SRV_PID=""
KEEP_TMP="${KEEP_TMP:-0}"
# Isolation: never inherit a caller's registry/anyhome switches from the env.
unset INIT_PRO_DISABLE_REGISTRY INIT_PRO_DISABLE_ANYHOME INIT_PRO_REGISTRY_ADDR \
  INIT_PRO_REGISTRY_TAG_RETENTION

RPORT="$(pick_port)"                 # registry plane (random free high port)
APORT="$(pick_port)"                 # apiserver; never collide with 6443
while [[ "$APORT" == "$RPORT" ]]; do APORT="$(pick_port)"; done
BASE="http://127.0.0.1:$APORT"
REG="http://127.0.0.1:$RPORT"
REPO="e2e/demo"
MANIFEST_TYPE="application/vnd.docker.distribution.manifest.v2+json"
CONFIG_TYPE="application/vnd.docker.container.image.v1+json"
LAYER_TYPE="application/vnd.docker.image.rootfs.diff.tar.gzip"

cleanup() {
  trap - EXIT INT TERM
  stop_pid "$SRV_PID"
  # Post-mortem copy, same convention as graceful-shutdown-test.sh.
  cat "$SRV_LOG" 2>/dev/null > /tmp/init-pro-registry.log || true
  if [[ "$KEEP_TMP" == "1" ]]; then
    echo "registry-e2e: artifacts kept at $TMP (KEEP_TMP=1)"
    return 0
  fi
  rm -rf "$TMP" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

dump_log() {
  echo "----- $SRV_LOG -----" >&2
  tail -60 "$SRV_LOG" >&2 2>/dev/null || true
}

# --- V2 wire helpers -------------------------------------------------------
upload_blob() { # FILE -> echoes the POST status (monolithic upload, 201 = ok)
  local f="$1" d
  d="$(digest_of "$f")"
  curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H 'Content-Type: application/octet-stream' \
    --data-binary "@$f" "$REG/v2/$REPO/blobs/uploads/?digest=$d"
}

put_manifest() { # FILE TAG -> echoes the PUT status
  curl -s -o "$TMP/put.out" -w '%{http_code}' -X PUT \
    -H "Content-Type: $MANIFEST_TYPE" \
    --data-binary "@$1" "$REG/v2/$REPO/manifests/$2"
}

# $1 = out file, $2 = optional annotation fragment, e.g. ,"annotations":{"tag":"v2"}
build_manifest() {
  local ann="${2:-}"
  printf '%s' "{\"schemaVersion\":2,\"mediaType\":\"$MANIFEST_TYPE\",\"config\":{\"mediaType\":\"$CONFIG_TYPE\",\"size\":$CFG_SIZE,\"digest\":\"$CFG_DIGEST\"},\"layers\":[{\"mediaType\":\"$LAYER_TYPE\",\"size\":$LAYER_SIZE,\"digest\":\"$LAYER_DIGEST\"}]$ann}" > "$1"
}

# tags/list body -> one tag per line (needs only sed/tr, no jq/python3)
list_tags() {
  sed -n 's/.*"tags":\[\([^]]*\)\].*/\1/p' "$1" | tr ',' '\n' | tr -d '"' \
    | sed 's/^[[:space:]]*//; s/[[:space:]]*$//' | grep -v '^$' || true
}

tags_snapshot() { # FILE -> curls tags/list into FILE, echoes the tag count
  curl -s "$REG/v2/$REPO/tags/list" > "$1" || true
  list_tags "$1" | wc -l | tr -d ' '
}

# --- gate runner -----------------------------------------------------------
WHY=""
run_gate() { # ID DESC FN -> per-gate ok/bad line; FN sets WHY on failure
  local id="$1" desc="$2" fn="$3"
  WHY=""
  if "$fn"; then
    ok "$id  $desc"
  else
    bad "$id  $desc (${WHY:-failed})"
    dump_log
  fi
}

# --- R1: boot + V2 ping ----------------------------------------------------
gate_r1() {
  # Isolation: pin the apiserver to a free high port (never steal a real
  # 6443) and switch the anyhome NFS plane off (its default 11111 collides
  # with other gates). Everything else keeps the default server boot.
  "$BIN" server \
    --data-dir "$DATA" \
    --token e2e-registry-token \
    --bind-address 127.0.0.1 \
    --https-listen-port "$APORT" \
    --disable-anyhome \
    --registry-addr "127.0.0.1:$RPORT" \
    --registry-tag-retention 10 >"$SRV_LOG" 2>&1 &
  SRV_PID=$!
  local i
  for i in $(seq 1 200); do
    curl -sf -o /dev/null "$BASE/api/v1" && break
    kill -0 "$SRV_PID" 2>/dev/null || { WHY="server exited during boot (see $SRV_LOG)"; return 1; }
    sleep 0.1
  done
  # Spec poll: ~50 x 200ms against the V2 ping endpoint.
  local up=0
  for i in $(seq 1 50); do
    if curl -sf -o /dev/null "$REG/v2/"; then up=1; break; fi
    kill -0 "$SRV_PID" 2>/dev/null || { WHY="server exited while waiting for /v2/"; return 1; }
    sleep 0.2
  done
  [[ "$up" == "1" ]] || { WHY="GET $REG/v2/ never returned 200"; return 1; }
  local hdrs
  hdrs="$(curl -s -D - -o /dev/null "$REG/v2/")"
  grep -qi '^docker-distribution-api-version:[[:space:]]*registry/2\.0' <<<"$hdrs" \
    || { WHY="GET /v2/ lacks Docker-Distribution-API-Version: registry/2.0"; return 1; }
  [[ -d "$DATA/registry/blobs" ]] \
    || { WHY="$DATA/registry/blobs missing (data-dir registry layout not created)"; return 1; }
  return 0
}

# --- R2: offline image fabrication + monolithic push -----------------------
gate_r2() {
  mkdir -p "$TMP/layerroot"
  tar -C "$TMP/layerroot" -cf "$TMP/layer.tar" . || { WHY="tar failed"; return 1; }
  gzip -n -c "$TMP/layer.tar" > "$TMP/layer.tar.gz" || { WHY="gzip failed"; return 1; }
  local layer_diff
  layer_diff="$(digest_of "$TMP/layer.tar")"          # UNCOMPRESSED diff_id
  LAYER_DIGEST="$(digest_of "$TMP/layer.tar.gz")"
  LAYER_SIZE="$(size_of "$TMP/layer.tar.gz")"
  printf '%s' "{\"architecture\":\"amd64\",\"os\":\"linux\",\"rootfs\":{\"type\":\"layers\",\"diff_ids\":[\"$layer_diff\"]},\"config\":{}}" \
    > "$TMP/config.json"
  CFG_DIGEST="$(digest_of "$TMP/config.json")"
  CFG_SIZE="$(size_of "$TMP/config.json")"

  local f code
  for f in "$TMP/config.json" "$TMP/layer.tar.gz"; do
    code="$(upload_blob "$f")"
    [[ "$code" == "201" ]] || { WHY="POST blobs/uploads/?digest=$(digest_of "$f") -> $code (expected 201)"; return 1; }
  done
  build_manifest "$TMP/manifest-v1.json" ""
  code="$(put_manifest "$TMP/manifest-v1.json" v1)"
  [[ "$code" == "201" ]] || { WHY="PUT manifests/v1 -> $code (expected 201): $(head -c 200 "$TMP/put.out")"; return 1; }
  return 0
}

# --- R3: manifest fetch by tag + by digest ---------------------------------
gate_r3() {
  local body="$TMP/get-tag.json" hdrs="$TMP/get-tag.hdr" code
  code="$(curl -s -o "$body" -D "$hdrs" -w '%{http_code}' "$REG/v2/$REPO/manifests/v1")"
  [[ "$code" == "200" ]] || { WHY="GET manifests/v1 -> $code (expected 200)"; return 1; }
  local want hdr
  want="$(digest_of "$TMP/manifest-v1.json")"
  hdr="$(tr -d '\r' < "$hdrs" | awk 'tolower($1)=="docker-content-digest:"{print $2}' | head -1)"
  [[ "$hdr" == "$want" ]] || { WHY="Docker-Content-Digest '$hdr' != sha256sum(manifest) '$want'"; return 1; }
  cmp -s "$body" "$TMP/manifest-v1.json" \
    || { WHY="GET by tag returned different bytes than were pushed"; return 1; }
  code="$(curl -s -o "$TMP/get-digest.json" -D "$TMP/get-digest.hdr" -w '%{http_code}' "$REG/v2/$REPO/manifests/$want")"
  [[ "$code" == "200" ]] || { WHY="GET manifests/$want -> $code (expected 200)"; return 1; }
  cmp -s "$TMP/get-digest.json" "$TMP/manifest-v1.json" \
    || { WHY="GET by digest returned different bytes than GET by tag"; return 1; }
  return 0
}

# --- R4: tag retention keeps the newest 10 ---------------------------------
gate_r4() {
  local i code
  for i in $(seq 2 12); do
    build_manifest "$TMP/manifest-v$i.json" ",\"annotations\":{\"tag\":\"v$i\"}"
    code="$(put_manifest "$TMP/manifest-v$i.json" "v$i")"
    [[ "$code" == "201" ]] || { WHY="PUT manifests/v$i -> $code (expected 201)"; return 1; }
  done
  local n
  n="$(tags_snapshot "$TMP/tags-r4.json")"
  [[ "$n" == "10" ]] || { WHY="tags/list has $n tags, expected 10: $(tr '\n' ' ' < <(list_tags "$TMP/tags-r4.json"))"; return 1; }
  for i in $(seq 3 12); do
    list_tags "$TMP/tags-r4.json" | grep -qx "v$i" || { WHY="v$i missing from tags/list (retention kept the wrong window)"; return 1; }
  done
  for i in 1 2; do
    ! list_tags "$TMP/tags-r4.json" | grep -qx "v$i" || { WHY="v$i survived retention (expected pruned)"; return 1; }
  done
  digest_of "$TMP/manifest-v12.json" > "$TMP/digest-v12"
  return 0
}

# --- R5: keep.json patterns are never pruned and never counted -------------
gate_r5() {
  mkdir -p "$DATA/registry"
  printf '%s' '["e2e/demo:pinned*"]' > "$DATA/registry/keep.json"   # hot-reloaded
  local code i
  build_manifest "$TMP/manifest-pinned.json" ',"annotations":{"tag":"pinned-old"}'
  code="$(put_manifest "$TMP/manifest-pinned.json" pinned-old)"
  [[ "$code" == "201" ]] || { WHY="PUT manifests/pinned-old -> $code (expected 201)"; return 1; }
  for i in $(seq 1 10); do
    build_manifest "$TMP/manifest-r$i.json" ",\"annotations\":{\"tag\":\"r$i\"}"
    code="$(put_manifest "$TMP/manifest-r$i.json" "r$i")"
    [[ "$code" == "201" ]] || { WHY="PUT manifests/r$i -> $code (expected 201)"; return 1; }
  done
  local n
  n="$(tags_snapshot "$TMP/tags-r5.json")"
  [[ "$n" == "11" ]] || { WHY="tags/list has $n tags, expected 11 (pinned-old + newest 10): $(tr '\n' ' ' < <(list_tags "$TMP/tags-r5.json"))"; return 1; }
  list_tags "$TMP/tags-r5.json" | grep -qx "pinned-old" \
    || { WHY="pinned-old was pruned despite keep.json pin"; return 1; }
  for i in $(seq 1 10); do
    list_tags "$TMP/tags-r5.json" | grep -qx "r$i" || { WHY="rolling tag r$i missing from tags/list"; return 1; }
  done
  ! list_tags "$TMP/tags-r5.json" | grep -qx "v12" \
    || { WHY="v12 still present: pinned-old counted toward the 10 (should be excluded)"; return 1; }
  digest_of "$TMP/manifest-r10.json" > "$TMP/digest-r10"
  return 0
}

# --- R6: delete semantics --------------------------------------------------
gate_r6() {
  local d12 d10 code body out
  d12="$(cat "$TMP/digest-v12")"
  d10="$(cat "$TMP/digest-r10")"
  # v12 was pruned by R5's ten rollovers: its manifest file is gone and no tag
  # points at the digest, so DELETE answers 404 MANIFEST_UNKNOWN (C4) instead
  # of the old blanket 202.
  out="$(curl -s -w '\n%{http_code}' -X DELETE "$REG/v2/$REPO/manifests/$d12")"
  code="${out##*$'\n'}"
  body="${out%$'\n'*}"
  [[ "$code" == "404" ]] || { WHY="DELETE unknown/pruned digest $d12 -> $code (expected 404 MANIFEST_UNKNOWN)"; return 1; }
  grep -q 'MANIFEST_UNKNOWN' <<<"$body" \
    || { WHY="DELETE unknown/pruned digest $d12 -> 404 but body lacks MANIFEST_UNKNOWN: $(head -c 200 <<<"$body")"; return 1; }
  code="$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$REG/v2/$REPO/manifests/pinned-old")"
  [[ "$code" == "400" ]] || { WHY="DELETE manifests/pinned-old (by tag) -> $code (expected 400)"; return 1; }
  # Live digest: the tag that points at it must disappear with the manifest.
  code="$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$REG/v2/$REPO/manifests/$d10")"
  [[ "$code" == "202" ]] || { WHY="DELETE live digest r10 -> $code (expected 202)"; return 1; }
  tags_snapshot "$TMP/tags-r6.json" >/dev/null
  ! list_tags "$TMP/tags-r6.json" | grep -qx "r10" \
    || { WHY="tag r10 survived the delete of its manifest digest"; return 1; }
  list_tags "$TMP/tags-r6.json" | grep -qx "pinned-old" \
    || { WHY="pinned-old vanished after an unrelated digest delete"; return 1; }
  return 0
}

# --- R7: best-effort real pull through the CRI stack -----------------------
# Manual equivalent (once a containerd + crictl pair is reachable, e.g. the
# server+agent boot from scripts/agent-exec-e2e.sh with the agent's
# --containerd-addr socket exported as CONTAINER_RUNTIME_ENDPOINT):
#   crictl pull 127.0.0.1:$RPORT/e2e/demo:v3
gate_r7() {
  local code
  # R5's rollovers evicted v3; re-push the identical (v3-annotated) manifest
  # so the documented pull target exists at pull time.
  [[ -f "$TMP/manifest-v3.json" ]] || { WHY="manifest-v3.json unavailable (R4 did not complete)"; return 1; }
  code="$(put_manifest "$TMP/manifest-v3.json" v3)"
  [[ "$code" == "201" ]] || { WHY="re-PUT manifests/v3 -> $code (expected 201)"; return 1; }
  if ! crictl pull "127.0.0.1:$RPORT/$REPO:v3" >"$TMP/crictl-pull.log" 2>&1; then
    WHY="crictl pull failed: $(tail -3 "$TMP/crictl-pull.log" | tr '\n' ' ')"
    return 1
  fi
  return 0
}

# --- run the gates ---------------------------------------------------------
run_gate R1 "server booted with --registry-addr and /v2/ answers 200 + registry/2.0 header" gate_r1
if (( FAIL > 0 )); then
  echo "registry-e2e: aborted after boot failure (later gates need a live registry)" >&2
  summary
  exit 1
fi
run_gate R2 "fabricated docker-v2 image pushed offline into $REPO:v1 (blobs + manifest 201)" gate_r2
run_gate R3 "manifest pull by tag + by digest (200, Docker-Content-Digest == sha256sum, bytes identical)" gate_r3
run_gate R4 "tag retention: 12 pushes => tags/list holds exactly v3..v12" gate_r4
run_gate R5 "keep.json pin: pinned-old survives 10 rollovers and is not counted (total 11)" gate_r5
run_gate R6 "delete semantics: unknown digest -> 404 MANIFEST_UNKNOWN, tag -> 400, live digest -> 202" gate_r6

if [[ -n "${INIT_PRO_E2E_CONTAINERD:-}" ]] && command -v crictl >/dev/null 2>&1; then
  run_gate R7 "real pull through crictl (127.0.0.1:$RPORT/$REPO:v3)" gate_r7
else
  if [[ -z "${INIT_PRO_E2E_CONTAINERD:-}" ]]; then
    skip "R7  containerd pull leg disabled (set INIT_PRO_E2E_CONTAINERD=1 with a live runtime; manual: crictl pull 127.0.0.1:$RPORT/e2e/demo:v3)"
  else
    skip "R7  crictl not on PATH (manual: crictl pull 127.0.0.1:$RPORT/e2e/demo:v3)"
  fi
fi

summary
if (( FAIL > 0 )); then
  echo "registry-e2e: FAILED ($FAIL gate(s)) — server log: /tmp/init-pro-registry.log" >&2
  exit 1
fi
echo "registry-e2e: all required gates passed (R1..R6)"
