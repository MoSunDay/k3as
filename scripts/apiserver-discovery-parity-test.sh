#!/usr/bin/env bash
# T1.2a acceptance: init-pro server serves byte-correct Kubernetes API discovery
# over HTTP on the loopback. End-to-end via curl against a real `init-pro server`
# process (the in-process byte-fidelity is covered by the rust integration test).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
command -v curl >/dev/null || { echo "error: curl not installed" >&2; exit 1; }

PORT="${INIT_PRO_API_PORT:-17443}"        # non-6443 to avoid clashing with host kubelet
DD="$(mktemp -d)"
trap 'rm -rf "$DD"' EXIT

ok()   { echo "OK   $*"; }
bad()  { echo "FAIL $*" >&2; exit 1; }

# Start the server on a unique loopback port, plain HTTP (TLS is T1.3, Q11).
"$BIN" server --data-dir "$DD" --bind-address 127.0.0.1 --https-listen-port "$PORT" \
    >/tmp/init-pro-apiserver.log 2>&1 &
SRV=$!
cleanup() { kill "$SRV" 2>/dev/null || true; wait "$SRV" 2>/dev/null || true; rm -rf "$DD"; }
trap cleanup EXIT

# Wait until the apiserver reports it is listening.
for _ in $(seq 1 50); do
  grep -q "discovery listening" /tmp/init-pro-apiserver.log && break
  sleep 0.1
done
grep -q "discovery listening" /tmp/init-pro-apiserver.log || bad "apiserver never reported listening"

BASE="http://127.0.0.1:${PORT}"

# /api -> APIVersions, must list the core "v1" version.
body="$(curl -fsS "$BASE/api")"
echo "$body" | grep -q '"v1"' || bad "/api missing v1 version: $body"
echo "$body" | grep -q '"serverAddress"' || bad "/api missing serverAddress: $body"
ok "/api serves APIVersions with v1"

# /apis -> APIGroupList, must include the init-pro.io group.
body="$(curl -fsS "$BASE/apis")"
echo "$body" | grep -q '"init-pro.io"' || bad "/apis missing init-pro.io group: $body"
ok "/apis serves APIGroupList with init-pro.io"

# /api/v1 -> core/v1 resource index, must list pods + namespaces.
body="$(curl -fsS "$BASE/api/v1")"
echo "$body" | grep -q '"pods"' || bad "/api/v1 missing pods: $body"
echo "$body" | grep -q '"namespaces"' || bad "/api/v1 missing namespaces: $body"
ok "/api/v1 serves core/v1 APIResourceList (pods, namespaces)"

# /apis/init-pro.io/v1 -> the CRD resource index, must list luarouters.
body="$(curl -fsS "$BASE/apis/init-pro.io/v1")"
echo "$body" | grep -q '"luarouters"' || bad "/apis/init-pro.io/v1 missing luarouters: $body"
ok "/apis/init-pro.io/v1 serves APIResourceList (luarouters)"

# Content-Type is application/json everywhere (decision Q10).
ct="$(curl -fsS -o /dev/null -w '%{content_type}' "$BASE/api")"
case "$ct" in
  application/json*) ok "/api content-type is application/json" ;;
  *) bad "/api content-type is $ct (expected application/json)" ;;
esac

# Unknown group/version -> 404 (upstream parity).
code="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/apis/fabricated.io/v9beta1")"
[[ "$code" == "404" ]] || bad "unknown group/version returned $code (expected 404)"
ok "/apis/fabricated.io/v9beta1 -> 404"

# --disable-apiserver must NOT open the port.
"$BIN" server --data-dir "$DD" --bind-address 127.0.0.1 --https-listen-port $((PORT+1)) \
    --disable-apiserver >/tmp/init-pro-noapi.log 2>&1 &
NOSRV=$!
trap 'cleanup; kill "$NOSRV" 2>/dev/null || true; wait "$NOSRV" 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
  grep -q "apiserver disabled by flag" /tmp/init-pro-noapi.log && break
  sleep 0.1
done
if curl -fsS -o /dev/null "http://127.0.0.1:$((PORT+1))/api" 2>/dev/null; then
  bad "--disable-apiserver still opened the port"
fi
ok "--disable-apiserver keeps the port closed"

echo "ALL discovery-parity checks passed"
