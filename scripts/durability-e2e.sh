#!/usr/bin/env bash
# Sprint 19 / T2.3 (Q29): durability across restart for the libsql/SQLite
# datastore backend. Boots a real init-pro server on
# `server --datastore-endpoint sqlite://<path>`, seeds objects, drains it
# cleanly, restarts it on the SAME DSN, and asserts:
#
#   D1  boot + seed      — SQLite backend actually in use, objects created
#   D2  clean drain      — SIGTERM exits 0 within the deadline, WAL state sane
#   D3  persistence      — every object survives with its resourceVersion
#                          IDENTICAL (no rewrite, no reset of revision
#                          bookkeeping)
#   D4  continuity       — the revision counter resumed (strictly greater
#                          RVs, never reused/regressed) + watch historical
#                          replay across the restart (persisted event table)
#                          + controllers/informer resync smoke (stable GET,
#                          no panic)
#
# kubectl's v1 surface here is `rollout status` only (T3.1b/Q21), so the
# script drives the apiserver REST API with curl over plain HTTP (JSON wire
# Q10; TLS is T1.3). resourceVersion is extracted with grep — the gate needs
# nothing but curl + coreutils, no python3.
#
# Usage: scripts/durability-e2e.sh
#   INIT_PRO_BIN=...  reuse a pre-built binary (default: target/debug/init-pro)
#
# Requires a PRE-BUILT binary (`cargo build --workspace --locked`). No vendor
# bundle, no root, no agent — runs everywhere CI does.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl >/dev/null || { echo "error: curl is required" >&2; exit 1; }

ok()   { echo "OK   $*"; }
fail() {
  echo "FAIL $*" >&2
  for f in "$LOG1" "$LOG2"; do
    [[ -f "$f" ]] && { echo "----- $f -----" >&2; cat "$f" >&2; }
  done
  exit 1
}

# --- setup -------------------------------------------------------------
DD="$(mktemp -d)"
LOG1="$DD/server-1.log"   # boot 1 (D1-D2)
LOG2="$DD/server-2.log"   # boot 2 (D3-D4); kept separate so readiness
                          # greps only see the current boot.
SRV=""
cleanup() {
  [[ -n "$SRV" ]] && { kill "$SRV" 2>/dev/null || true; wait "$SRV" 2>/dev/null || true; }
  # Post-mortem copy, same convention as graceful-shutdown-test.sh.
  cat "$LOG1" "$LOG2" 2>/dev/null > /tmp/init-pro-durability.log || true
  rm -rf "$DD"
}
trap cleanup EXIT

PORT=16499
BASE="http://127.0.0.1:$PORT"
DSN="sqlite://$DD/server/state.db"
DB="$DD/server/state.db"

# Wait until the server logs readiness (then answers HTTP), ~10s budget —
# the graceful-shutdown-test.sh pattern.
wait_ready() {
  local log="$1"
  for _ in $(seq 1 100); do
    grep -q "ready" "$log" 2>/dev/null && break
    sleep 0.1
  done
  grep -q "ready" "$log" || fail "server never logged readiness (see $log)"
  for _ in $(seq 1 50); do
    curl -s -o /dev/null "$BASE/api/v1" && break
    sleep 0.1
  done
  curl -s -o /dev/null "$BASE/api/v1" || fail "apiserver never answered on $BASE"
}

# Extract metadata.resourceVersion from a single-object GET (grep-only).
rv_of() {
  curl -fsS "$BASE$1" | grep -o '"resourceVersion":"[0-9][0-9]*"' | head -1 | grep -o '[0-9][0-9]*'
}

# SIGTERM, wait <=10s for exit, set DRAIN_CODE to the exit status
# (-1 = missed deadline). NOTE: must run in the MAIN shell — inside a
# command substitution the parent cannot reap the child, it stays a
# zombie and `kill -0` keeps succeeding (graceful-shutdown-test.sh runs
# its loop the same way, directly in the top-level shell).
DRAIN_CODE=-1
drain() {
  local pid="$1"
  DRAIN_CODE=-1
  kill -TERM "$pid" 2>/dev/null || true
  local deadline=$((SECONDS + 10))
  while (( SECONDS < deadline )); do
    if ! kill -0 "$pid" 2>/dev/null; then
      DRAIN_CODE=0; wait "$pid" || DRAIN_CODE=$?
      return
    fi
    sleep 0.1
  done
}

TMP_NS="$(mktemp)"; TMP_CM="$(mktemp)"; TMP_DEP="$(mktemp)"; TMP_CM2="$(mktemp)"
printf '%s' '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"dur"}}' > "$TMP_NS"
printf '%s' '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"dur-cm"},"data":{"k":"v"}}' > "$TMP_CM"
# Deployment shape mirrors golden-conformance.sh G17 (minimal valid apps/v1).
printf '%s' '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"dur-dep","namespace":"dur"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"dur-web"}},"template":{"metadata":{"labels":{"app":"dur-web"}},"spec":{"containers":[{"name":"web","image":"nginx:1.29","ports":[{"containerPort":80}]}]}}}}' > "$TMP_DEP"
printf '%s' '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"dur-cm-2"},"data":{"k2":"v2"}}' > "$TMP_CM2"

post_code() { # $1 body-file  $2 path
  curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" \
    --data-binary "@$1" "$BASE$2" 2>/dev/null || true
}
get_code() { # $1 path
  curl -s -o /dev/null -w '%{http_code}' "$BASE$1" 2>/dev/null || true
}

# --- D1: boot + seed ----------------------------------------------------
echo "## durability e2e — SQLite datastore across restart (port $PORT)"
"$BIN" server --data-dir "$DD" --datastore-endpoint "$DSN" \
  --https-listen-port "$PORT" --bind-address 127.0.0.1 >"$LOG1" 2>&1 &
SRV=$!
wait_ready "$LOG1"

grep -q "SQLite datastore" "$LOG1" || fail "D1 log does not mention the SQLite datastore backend"
[[ -f "$DB" ]] || fail "D1 $DB was not created"
ok "D1  booted on sqlite://<data-dir>/server/state.db (SQLite datastore, T2.3/Q29)"

C="$(post_code "$TMP_NS"  /api/v1/namespaces)"
[[ "$C" == "201" ]] || fail "D1 POST namespace -> $C (expected 201)"
C="$(post_code "$TMP_CM"  /api/v1/namespaces/dur/configmaps)"
[[ "$C" == "201" ]] || fail "D1 POST configmap -> $C (expected 201)"
C="$(post_code "$TMP_DEP" /apis/apps/v1/namespaces/dur/deployments)"
[[ "$C" == "201" ]] || fail "D1 POST deployment -> $C (expected 201)"

RV_NS="$(rv_of /api/v1/namespaces/dur)"
RV_CM="$(rv_of /api/v1/namespaces/dur/configmaps/dur-cm)"
RV_DEP="$(rv_of /apis/apps/v1/namespaces/dur/deployments/dur-dep)"
[[ -n "$RV_NS" && -n "$RV_CM" && -n "$RV_DEP" ]] || fail "D1 could not read seeded resourceVersions (ns=$RV_NS cm=$RV_CM dep=$RV_DEP)"
MAX_PRE="$RV_NS"
if (( RV_CM > MAX_PRE ));  then MAX_PRE="$RV_CM"; fi
if (( RV_DEP > MAX_PRE )); then MAX_PRE="$RV_DEP"; fi
ok "D1  seeded namespace+configmap+deployment (rv ns=$RV_NS cm=$RV_CM dep=$RV_DEP, max=$MAX_PRE)"

# --- D2: clean drain ----------------------------------------------------
drain "$SRV"
SRV=""
[[ "$DRAIN_CODE" == "0" ]] || fail "D2 server exited with $DRAIN_CODE (expected 0) or did not stop in 10s"
if [[ -f "$DB-wal" ]]; then
  echo "     note: $DB-wal still present after drain (not checkpointed; informational only)"
else
  echo "     note: WAL checkpointed on drain ($DB-wal absent)"
fi
ok "D2  server drained cleanly on SIGTERM (exit 0 within deadline)"

# --- D3: restart + persistence -----------------------------------------
"$BIN" server --data-dir "$DD" --datastore-endpoint "$DSN" \
  --https-listen-port "$PORT" --bind-address 127.0.0.1 >"$LOG2" 2>&1 &
SRV=$!
wait_ready "$LOG2"

grep -q "SQLite datastore" "$LOG2" || fail "D3 second boot does not mention the SQLite datastore backend"

C="$(get_code /api/v1/namespaces/dur)"
[[ "$C" == "200" ]] || fail "D3 GET namespace after restart -> $C (expected 200)"
C="$(get_code /api/v1/namespaces/dur/configmaps/dur-cm)"
[[ "$C" == "200" ]] || fail "D3 GET configmap after restart -> $C (expected 200)"
C="$(get_code /apis/apps/v1/namespaces/dur/deployments/dur-dep)"
[[ "$C" == "200" ]] || fail "D3 GET deployment after restart -> $C (expected 200)"

RV_NS2="$(rv_of /api/v1/namespaces/dur)"
RV_CM2a="$(rv_of /api/v1/namespaces/dur/configmaps/dur-cm)"
RV_DEP2="$(rv_of /apis/apps/v1/namespaces/dur/deployments/dur-dep)"
[[ "$RV_NS2" == "$RV_NS" ]]  || fail "D3 namespace resourceVersion changed across restart ($RV_NS -> $RV_NS2; objects were rewritten)"
[[ "$RV_CM2a" == "$RV_CM" ]] || fail "D3 configmap resourceVersion changed across restart ($RV_CM -> $RV_CM2a; objects were rewritten)"
[[ "$RV_DEP2" == "$RV_DEP" ]] || fail "D3 deployment resourceVersion changed across restart ($RV_DEP -> $RV_DEP2; objects were rewritten)"
ok "D3  namespace/configmap/deployment survived restart with identical resourceVersions"

# --- D4: continuity + watch replay -------------------------------------
C="$(post_code "$TMP_CM2" /api/v1/namespaces/dur/configmaps)"
[[ "$C" == "201" ]] || fail "D4 POST second configmap -> $C (expected 201)"
RV_CM2="$(rv_of /api/v1/namespaces/dur/configmaps/dur-cm-2)"
[[ -n "$RV_CM2" ]] || fail "D4 could not read dur-cm-2 resourceVersion"
if (( RV_CM2 > MAX_PRE )); then
  ok "D4  post-restart create got rv=$RV_CM2 > pre-restart max=$MAX_PRE (revision counter resumed, never regressed)"
else
  fail "D4 post-restart create got rv=$RV_CM2 <= pre-restart max=$MAX_PRE (revision counter reset/regressed)"
fi

# Watch replay from the persisted event table: the ADDED for dur-cm-2 must
# be replayed when starting at the pre-restart RV_CM. curl is bounded by
# --max-time (exit 28 is expected once the replay drains and the stream
# idles) — same suppression as golden G15.
WATCH_BODY="$(curl -sN --max-time 5 \
  "$BASE/api/v1/namespaces/dur/configmaps?watch=1&resourceVersion=$RV_CM" 2>/dev/null || true)"
if printf '%s' "$WATCH_BODY" | grep -q '"dur-cm-2"'; then
  ok "D4  watch from pre-restart rv=$RV_CM replayed the dur-cm-2 ADDED (event table persisted)"
else
  fail "D4 watch from rv=$RV_CM did not replay dur-cm-2; body: $WATCH_BODY"
fi

# Controllers/informer resync smoke: after ~1.5s of reconciling the restored
# world, the deployment GET is still stable and neither boot panicked.
sleep 1.5
C="$(get_code /apis/apps/v1/namespaces/dur/deployments/dur-dep)"
[[ "$C" == "200" ]] || fail "D4 deployment GET unstable after resync ($C)"
if grep -q "panic" "$LOG1" || grep -q "panic" "$LOG2"; then
  fail "D4 a server log contains 'panic' (crash loop after restart)"
fi
ok "D4  controllers/informers resynced cleanly (deployment stable, no panic)"

rm -f "$TMP_NS" "$TMP_CM" "$TMP_DEP" "$TMP_CM2"
echo
echo "ALL durability checks passed (D1..D4)"
