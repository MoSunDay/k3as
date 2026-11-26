#!/usr/bin/env bash
# 长稳清扫 (long-run sweep): a server-only stability soak for `init-pro server`.
#
# One run boots the server on the durable datastore (T2.3, Q33:
# `--datastore-endpoint rdb://<dir>`), drives continuous API churn through the
# apiserver REST surface (T1.2) into the embedded registry (T6.3, Q40) and the
# pipeline plane (T8.3, Q56), samples the server process, and then proves that
# a clean shutdown is timely and that the data survives a restart:
#
#   SK1 boot      — rdb datastore + registry (`/v2/`) + pipeline CRD up, with
#                   an unreachable docker configured (archive-mode builds only)
#   SK2 churn     — >= SOAK_SECONDS/2 iterations completed, ERRS == 0
#   SK3 memory    — last-sample RSS <= first-sample RSS + 64 MiB
#   SK4 fds       — last <= first + 32 and last < 1024
#   SK5 threads   — last <= first + 16
#   SK6 shutdown  — SIGTERM exits within 20 s
#   SK7 restart   — keeper ConfigMap, the FIRST bumped registry tag and the
#                   ServicePipeline status.observedTag all survive a restart
#
# Usage: scripts/soak-e2e.sh
#   SOAK_SECONDS=45    battery profile (default 600 standalone)
#   INIT_PRO_BIN=...   reuse a pre-built binary
#
# Self-SKIPs (exit 0, first stdout line "SKIP ...") when a precondition is
# missing: no binary / no curl / no python3 / no tar. No root, no vendor
# bundle, no docker: `INIT_PRO_DOCKER` points at a path that does not exist,
# so the pipeline plane is ON but no docker is ever invoked (every Build uses
# `spec.source.archive`, the docker-free import path of T8.3/Q56).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

# --- preconditions (the SKIP contract) ------------------------------------
MISSING=()
[[ -x "$BIN" ]] || MISSING+=("no-binary")
command -v curl   >/dev/null 2>&1 || MISSING+=("no-curl")
command -v python3 >/dev/null 2>&1 || MISSING+=("no-python3")
command -v tar    >/dev/null 2>&1 || MISSING+=("no-tar")
if (( ${#MISSING[@]} > 0 )); then
  echo "SKIP soak-e2e: missing ${MISSING[*]} (needs a pre-built binary, curl, python3 and tar)"
  exit 0
fi

PASS_N=0; FAIL_N=0
ok()  { echo "OK   $*"; PASS_N=$((PASS_N + 1)); }
bad() { echo "FAIL $*" >&2; FAIL_N=$((FAIL_N + 1)); }

pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
    || echo $((20000 + RANDOM % 10000))
}

stop_pid() { # $1 pid — TERM, bounded wait, then KILL (never leaves a server)
  local p="$1"
  [[ -n "$p" ]] || return 0
  kill -TERM "$p" 2>/dev/null || true
  for _ in $(seq 1 50); do
    kill -0 "$p" 2>/dev/null || return 0
    sleep 0.1
  done
  kill -KILL "$p" 2>/dev/null || true
  wait "$p" 2>/dev/null || true
}

# --- duration / sampling profile ------------------------------------------
SOAK_SECONDS="${SOAK_SECONDS:-600}"
# Floor of 20 s: with the 5 s minimum sample interval that still yields >=3
# samples (the spec asks for >=3 even on the 45 s battery profile).
if ! [[ "$SOAK_SECONDS" =~ ^[0-9]+$ ]] || (( SOAK_SECONDS < 20 )); then
  SOAK_SECONDS=20
fi
INTERVAL=$(( SOAK_SECONDS / 4 ))
if (( INTERVAL < 5 )); then INTERVAL=5; fi
if (( INTERVAL > 30 )); then INTERVAL=30; fi

DD="$(mktemp -d)"
DATA="$DD/data"
LOG1="$DD/server-1.log"     # boot 1 (SK1-SK6)
LOG2="$DD/server-2.log"     # boot 2 (SK7); separate so readiness greps stay honest
SRV=""
cleanup() {
  if [[ -n "$SRV" ]]; then stop_pid "$SRV"; SRV=""; fi
  cat "$LOG1" "$LOG2" 2>/dev/null > /tmp/init-pro-soak.log || true
  rm -rf "$DD" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

APORT="$(pick_port)"
RPORT="$(pick_port)"
while (( RPORT == APORT )); do RPORT="$(pick_port)"; done
BASE="http://127.0.0.1:$APORT"
REG="http://127.0.0.1:$RPORT"
NS="default"
SP="soak-sp"
DEP="soak-dep"
KEEPER="soak-keeper"
# Tag scheme: plain `b<i>` (NOT zero-padded) on purpose. The first soak run
# (batch F) found that the controller picked "newest child of ANY tag" by
# `(creationTimestamp, name)` -- and a Build carries no creationTimestamp, so
# `soak-sp-b9-x` sorted above `soak-sp-b10-x` and the one-Build-per-tag guard
# stormed ~34 Builds/s at the 9->10 rollover. Fixed by
# `plan::latest_build_for_tag` (filter by tag first); these tags keep the
# soak honest about that path and SK2 asserts no two Build children share a
# tag (a standalone 600 s run crosses the rollover; the unit test
# `latest_build_for_tag_ignores_other_tags_regardless_of_name_order` pins it).
FIRST_TAG="b1"   # the first bump happens on iteration 1 (see bump schedule)
LAST_TAG=""
ITERS=0; ERRS=0; BUILD_BUMPS=0; SAMPLES=0
S_RSS1=0; S_RSS2=0; S_FD1=0; S_FD2=0; S_TH1=0; S_TH2=0

# --- helpers ---------------------------------------------------------------
code_of() { # $1 method  $2 content-type  $3 body  $4 path [$5 max-time] -> HTTP status
  local method="$1" ct="$2" body="$3" path="$4" budget="${5:-15}"
  if [[ -n "$ct" ]]; then
    curl -s -o /dev/null -w '%{http_code}' -m "$budget" -X "$method" \
      -H "Content-Type: $ct" --data-binary "$body" "$BASE$path" 2>/dev/null || true
  else
    curl -s -o /dev/null -w '%{http_code}' -m "$budget" -X "$method" "$BASE$path" 2>/dev/null || true
  fi
}

expect() { # $1 expected-code  $2 actual  $3 what — a non-2xx API answer is an ERRS
  if [[ "$2" != "$1" ]]; then
    ERRS=$((ERRS + 1))
    echo "note: api error: $3 -> $2 (expected $1)" >&2
  fi
  return 0
}

json_get() { # $1 python-expr over `d` (the parsed body) — prints "" on failure
  python3 -c 'import sys,json
try:
    d=json.load(sys.stdin)
    print('"$1"')
except Exception:
    print("")' 2>/dev/null || true
}

sp_observed() { curl -s -m 10 "$BASE/apis/init-pro.io/v1/namespaces/$NS/servicepipelines/$SP" 2>/dev/null | json_get 'd.get("status",{}).get("observedTag","")'; }
reg_tags()    { curl -s -m 10 "$REG/v2/soak/app/tags/list" 2>/dev/null | json_get '" ".join(d.get("tags",[]))'; }
keeper_data() { curl -s -m 10 "$BASE/api/v1/namespaces/$NS/configmaps/$KEEPER" 2>/dev/null | json_get 'json.dumps(d.get("data",{}),sort_keys=True)'; }

boot_server() { # $1 log file — same data dir/DSN on both boots (SK7)
  # Q40 keep-forever pin for the FIRST bumped tag: the default newest-10 tag
  # retention would prune it over a long soak, and SK7 asserts that the very
  # first bump is still served after the restart.
  INIT_PRO_DOCKER=/nonexistent/docker-soak "$BIN" server \
    --data-dir "$DATA" \
    --token soak-e2e-token \
    --bind-address 127.0.0.1 \
    --https-listen-port "$APORT" \
    --disable-anyhome \
    --registry-addr "127.0.0.1:$RPORT" \
    --registry-keep-tag "soak/app:$FIRST_TAG" \
    --datastore-endpoint "rdb://$DATA/rdb" \
    --pipeline-dir "$DATA/pipeline" \
    >"$1" 2>&1 &
  SRV=$!
}

wait_ready() { # bounded /api/v1 poll (readiness recipe of durability-e2e.sh)
  local code
  for _ in $(seq 1 120); do
    code="$(curl -s -o /dev/null -w '%{http_code}' -m 3 "$BASE/api/v1" 2>/dev/null || true)"
    if [[ "$code" == "200" ]]; then return 0; fi
    kill -0 "$SRV" 2>/dev/null || return 1
    sleep 0.25
  done
  return 1
}

# --- fixture: a `docker save` archive (crates/controllers/tests/pipeline.rs
# write_fixture); GNU tar, three members, RepoTags are NOT validated against
# the pushed reference (crates/registry/src/tarimg.rs only parses them). ----
FIX="$DD/fixture"
mkdir -p "$FIX"
printf '%s' '[{"Config":"cfg.json","RepoTags":["soak/app:b0"],"Layers":["layer.tar"]}]' >"$FIX/manifest.json"
printf '%s' '{"architecture":"amd64","os":"linux"}' >"$FIX/cfg.json"
printf 'layer bytes for the soak e2e\n' >"$FIX/layer.tar"
tar -cf "$DD/fixture.tar" -C "$FIX" manifest.json cfg.json layer.tar

# --- boot + seed -----------------------------------------------------------
echo "## soak e2e — long-run stability sweep (SOAK_SECONDS=$SOAK_SECONDS, sample interval ${INTERVAL}s, port $APORT/$RPORT)"
boot_server "$LOG1"
if ! wait_ready; then
  bad "SK1 boot: server never answered on $BASE"
  cat "$LOG1" >&2 || true
  echo "FAIL soak-e2e (1 checks, 0s)" >&2
  exit 1
fi

KEEPER_BODY='{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"'"$KEEPER"'"},"data":{"k":"keeper-v1"}}'
KEEPER_WANT="$(python3 -c 'import json; print(json.dumps({"k": "keeper-v1"}, sort_keys=True))')"
DEP_BODY='{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"'"$DEP"'"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"soak-web"}},"template":{"metadata":{"labels":{"app":"soak-web"}},"spec":{"containers":[{"name":"web","image":"busybox:1.36"}]}}}}'
SP_BODY='{"apiVersion":"init-pro.io/v1","kind":"ServicePipeline","metadata":{"name":"'"$SP"'"},"spec":{"source":{"archive":"'"$DD"'/fixture.tar"},"image":{"repository":"127.0.0.1:'"$RPORT"'/soak/app","tag":"b0"}}}'
expect 201 "$(code_of POST application/json "$KEEPER_BODY" "/api/v1/namespaces/$NS/configmaps")" "create $KEEPER"
expect 201 "$(code_of POST application/json "$DEP_BODY" "/apis/apps/v1/namespaces/$NS/deployments")" "create $DEP"
expect 201 "$(code_of POST application/json "$SP_BODY" "/apis/init-pro.io/v1/namespaces/$NS/servicepipelines")" "create $SP"

# --- SK1: boot -------------------------------------------------------------
if grep -q "rdb datastore" "$LOG1" && [[ -d "$DATA/rdb" ]] \
   && [[ "$(curl -s -o /dev/null -w '%{http_code}' -m 10 "$REG/v2/" || true)" == "200" ]] \
   && [[ -n "$(curl -s -m 10 "$BASE/apis/init-pro.io/v1" | json_get '"servicepipelines" if any(r.get("name")=="servicepipelines" for r in d.get("resources",[])) else ""')" ]] \
   && [[ ! -x /nonexistent/docker-soak ]]; then
  ok "SK1 boot: rdb:// datastore + registry /v2/ + servicepipelines CRD up (docker unreachable)"
else
  bad "SK1 boot: rdb datastore / registry / pipeline CRD not all up (see $LOG1)"
fi

# --- churn loop ------------------------------------------------------------
T0=$SECONDS
DEADLINE=$(( T0 + SOAK_SECONDS ))
NEXT_SAMPLE=$(( T0 + INTERVAL ))
sample() { # RSS / fds / threads of the server pid + server log size
  local rss fds th logk
  rss="$(awk '/^VmRSS:/{print $2}' "/proc/$SRV/status" 2>/dev/null || echo 0)"
  fds="$(ls "/proc/$SRV/fd" 2>/dev/null | wc -l | tr -d ' ' || echo 0)"
  th="$(awk '/^Threads:/{print $2}' "/proc/$SRV/status" 2>/dev/null || echo 0)"
  logk="$(( $(stat -c %s "$LOG1" 2>/dev/null || echo 0) / 1024 ))"
  SAMPLES=$((SAMPLES + 1))
  if (( SAMPLES == 1 )); then S_RSS1=$rss; S_FD1=$fds; S_TH1=$th; fi
  S_RSS2=$rss; S_FD2=$fds; S_TH2=$th
  echo "note: sample $SAMPLES t=$((SECONDS - T0))s rss=$((rss / 1024))M fds=$fds threads=$th log=${logk}K iters=$ITERS errs=$ERRS"
}

while (( SECONDS < DEADLINE )); do
  ITERS=$((ITERS + 1))
  name="soak-cm-$ITERS"
  cm_create='{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"'"$name"'"},"data":{"k":"v0"}}'
  expect 201 "$(code_of POST application/json "$cm_create" "/api/v1/namespaces/$NS/configmaps")" "create $name"
  expect 200 "$(code_of PATCH application/merge-patch+json '{"data":{"k":"v1"}}' "/api/v1/namespaces/$NS/configmaps/$name")" "patch $name"
  expect 200 "$(code_of DELETE '' '' "/api/v1/namespaces/$NS/configmaps/$name")" "delete $name"
  # Deployment churn: alternate spec.replicas 1/2 (merge-patch, T1.2 item route)
  if (( ITERS % 2 == 0 )); then rep=2; else rep=1; fi
  expect 200 "$(code_of PATCH application/merge-patch+json '{"spec":{"replicas":'"$rep"'}}' "/apis/apps/v1/namespaces/$NS/deployments/$DEP")" "replicas=$rep"
  # Pipeline bump every ~15 iterations, the first one on iteration 1 so even a
  # 20 s profile exercises the archive-mode (docker-free) Build+push path.
  if (( ITERS % 15 == 1 )); then
    LAST_TAG="b$ITERS"
    BUILD_BUMPS=$((BUILD_BUMPS + 1))
    expect 200 "$(code_of PATCH application/merge-patch+json '{"spec":{"image":{"tag":"'"$LAST_TAG"'"}}}' "/apis/init-pro.io/v1/namespaces/$NS/servicepipelines/$SP")" "bump $LAST_TAG"
  fi
  # Bounded watch drain every ~10 iterations. `timeoutSeconds=2` is sent for
  # upstream parity, but the apiserver watch handler (T1.2b) does not implement
  # it yet -- the stream is closed by the CLIENT budget, so a 000 after the
  # 2 s drain is a successful drain, never an API error.
  if (( ITERS % 10 == 0 )); then
    wcode="$(code_of GET '' '' "/api/v1/namespaces/$NS/configmaps?watch=true&timeoutSeconds=2" 2)"
    if [[ "$wcode" != "200" && "$wcode" != "000" ]]; then
      ERRS=$((ERRS + 1)); echo "note: api error: watch -> $wcode" >&2
    fi
  fi
  if (( SECONDS >= NEXT_SAMPLE )); then
    sample
    NEXT_SAMPLE=$(( NEXT_SAMPLE + INTERVAL ))
  fi
  sleep 1
done

# Drain: let the LAST bump reach Promoted before shutdown (SK7 asserts it).
PROMOTED=""
if [[ -n "$LAST_TAG" ]]; then
  for _ in $(seq 1 60); do
    PROMOTED="$(sp_observed)"
    [[ "$PROMOTED" == "$LAST_TAG" ]] && break
    sleep 1
  done
fi

# --- SK2: churn (+ the one-Build-per-tag invariant) -----------------------
# Children are listed through the API and grouped by owner + tag: a tag with
# two or more Build children means the "newest child" guard regressed (the
# storm above). Bumps that were never observed by a reconcile legitimately
# leave no child, so only the per-tag multiplicity is asserted.
MIN_ITERS=$(( SOAK_SECONDS / 2 ))
BUILD_STORMS="$(curl -s -m 10 "$BASE/apis/init-pro.io/v1/namespaces/$NS/builds" | python3 -c 'import sys,json
try:
    items=json.load(sys.stdin).get("items", [])
    tags=[b["spec"]["image"]["tag"] for b in items
          if any(o.get("kind")=="ServicePipeline" and o.get("name")=="'"$SP"'" for o in b["metadata"].get("ownerReferences", []))]
    print(sum(1 for t in set(tags) if tags.count(t) > 1))
except Exception:
    print("unknown")' 2>/dev/null || echo unknown)"
BUILD_N="$(curl -s -m 10 "$BASE/apis/init-pro.io/v1/namespaces/$NS/builds" | json_get 'len(d.get("items", []))')"
if (( ITERS >= MIN_ITERS )) && (( ERRS == 0 )) && [[ "$BUILD_STORMS" == "0" ]]; then
  ok "SK2 churn: $ITERS iterations >= $MIN_ITERS (SOAK_SECONDS/2) with ERRS=$ERRS, bumps=$BUILD_BUMPS -> $BUILD_N Build children, no tag with 2+ children (last tag $LAST_TAG, observed $PROMOTED)"
else
  bad "SK2 churn: $ITERS iterations (bound $MIN_ITERS) ERRS=$ERRS bumps=$BUILD_BUMPS children=$BUILD_N tags-with-2+-children=$BUILD_STORMS (last tag $LAST_TAG, observed $PROMOTED)"
fi

# --- SK3/SK4/SK5: bounded growth ------------------------------------------
if (( SAMPLES >= 2 )); then
  if (( S_RSS2 <= S_RSS1 + 64 * 1024 )); then
    ok "SK3 memory: rss $((S_RSS1 / 1024))M -> $((S_RSS2 / 1024))M (bound +64M, $SAMPLES samples)"
  else
    bad "SK3 memory: rss $((S_RSS1 / 1024))M -> $((S_RSS2 / 1024))M exceeds +64M"
  fi
  if (( S_FD2 <= S_FD1 + 32 )) && (( S_FD2 < 1024 )); then
    ok "SK4 fds: $S_FD1 -> $S_FD2 (bound +32, <1024)"
  else
    bad "SK4 fds: $S_FD1 -> $S_FD2 (bound +32, <1024)"
  fi
  if (( S_TH2 <= S_TH1 + 16 )); then
    ok "SK5 threads: $S_TH1 -> $S_TH2 (bound +16)"
  else
    bad "SK5 threads: $S_TH1 -> $S_TH2 (bound +16)"
  fi
else
  bad "SK3 memory: only $SAMPLES samples taken"
  bad "SK4 fds: only $SAMPLES samples taken"
  bad "SK5 threads: only $SAMPLES samples taken"
fi

# --- SK6: graceful shutdown ------------------------------------------------
T_STOP=$SECONDS
kill -TERM "$SRV" 2>/dev/null || true
CODE=-1
for _ in $(seq 1 200); do
  if ! kill -0 "$SRV" 2>/dev/null; then CODE=0; wait "$SRV" 2>/dev/null || CODE=$?; break; fi
  sleep 0.1
done
DRAIN=$(( SECONDS - T_STOP ))
if [[ "$CODE" == "0" ]] && (( DRAIN <= 20 )); then
  ok "SK6 shutdown: SIGTERM -> exit 0 in ${DRAIN}s (bound 20s)"
  SRV=""
else
  bad "SK6 shutdown: exit=$CODE after ${DRAIN}s (bound 20s)"
  stop_pid "$SRV"; SRV=""
fi

# --- SK7: durability across a restart --------------------------------------
boot_server "$LOG2"
if wait_ready; then
  K_DATA="$(keeper_data)"
  TAGS="$(reg_tags)"
  OBS="$(sp_observed)"
  for _ in $(seq 1 30); do [[ "$OBS" == "$LAST_TAG" ]] && break; OBS="$(sp_observed)"; sleep 1; done
  if [[ "$K_DATA" == "$KEEPER_WANT" ]] \
     && [[ " $TAGS " == *" $FIRST_TAG "* ]] \
     && [[ -n "$LAST_TAG" ]] && [[ "$OBS" == "$LAST_TAG" ]]; then
    ok "SK7 restart: $KEEPER kept ($K_DATA), registry tags [$TAGS] keep $FIRST_TAG, observedTag=$OBS == last bump $LAST_TAG"
  else
    bad "SK7 restart: keeper=$K_DATA tags=[$TAGS] observedTag=$OBS (want $LAST_TAG, first bump $FIRST_TAG)"
  fi
else
  bad "SK7 restart: server never answered on the same data dir (see $LOG2)"
fi
stop_pid "$SRV"; SRV=""

# --- summary ----------------------------------------------------------------
CHECKS=$(( PASS_N + FAIL_N ))
echo
if (( FAIL_N == 0 )); then
  echo "PASS soak-e2e ($CHECKS checks, $(( SECONDS - T0 ))s)"
else
  echo "FAIL soak-e2e ($CHECKS checks, $(( SECONDS - T0 ))s)"
  echo "FAIL soak-e2e ($CHECKS checks, $(( SECONDS - T0 ))s)" >&2
  exit 1
fi
