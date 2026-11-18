#!/usr/bin/env bash
# T4.13 (Q53/Q54): end-to-end exercise of the REAL image pull chain —
# a docker-v2 image pushed to the server's embedded registry is pulled by
# the builtin engine's kubelet (manifest verify -> CAS blobs -> shared
# per-layer unpack) and RUNS as a Pod. Shared helpers live in
# scripts/lib/builtin-image-lib.sh.
#
# Cases:
#   I1 real image Pod: micro OCI image (busybox layer) pushed as
#      test/builtin-img:v1 -> Pod runs it with an explicit command ->
#      `kubectl logs` shows the marker.
#   I2 bad ref: unknown repo stays Pending (pull error must not kill the
#      agent loop).
#   I3 two-layer image with whiteouts (del/.wh.me.txt + sub/.wh..wh..opq)
#      pushed as test/whtwo:v1 -> Pod asserts the merged view: layers
#      spell "l1l2n", whiteouted entries are gone, the opaque dir holds
#      ONLY its new file (Q53: per-container overlayfs view, or the
#      fused-copy fallback when the kernel path is out).
#   I4 no-command Pod (M4): image Entrypoint = busybox httpd serving /www
#      — the image config is the launch source of truth. Asserts pod
#      Running, `curl 127.0.0.1:18080` (Q52 host netns) returns the
#      image's index.html, and `kubectl logs` shows the httpd request
#      line (-v). Pod deleted afterwards (port freed).
#   I5 image env vs pod env (M4): image Env IMG_ENV + Entrypoint echoing
#      ENVPOD-UP; pod adds MERGED_FROM with NO command. Asserts logs
#      show the marker and `kubectl exec ... env` sees BOTH vars
#      (image env survived, pod env appended last-wins).
#
# Preconditions: PRE-BUILT binary, real root, static busybox, curl +
# python3 (image fabrication + REST). Missing inputs => "SKIP
# builtin-image-e2e:" + exit 0 (Q25); a violated assertion is a hard FAIL.
#
# Usage: scripts/builtin-image-e2e.sh
#   INIT_PRO_BIN=...      reuse a specific binary (default target/debug/init-pro)
#   INIT_PRO_BUSYBOX=...  override the static busybox (default /usr/bin/busybox)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"
BUSYBOX="${INIT_PRO_BUSYBOX:-/usr/bin/busybox}"

skip() { echo "SKIP builtin-image-e2e: $*"; exit 0; }

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
[[ "$(id -u)" -eq 0 ]] || skip "requires real root (no user namespaces)"
[[ -x "$BUSYBOX" ]] || skip "no static busybox at $BUSYBOX"
command -v curl >/dev/null 2>&1 || skip "curl is required (registry push + REST)"
command -v python3 >/dev/null 2>&1 || skip "python3 is required (image fabrication)"
if ldd "$BUSYBOX" >/dev/null 2>&1; then
  skip "$BUSYBOX is dynamically linked; the builtin image needs a static busybox"
fi

# shellcheck source=scripts/lib/builtin-image-lib.sh
source "$SCRIPT_DIR/lib/builtin-image-lib.sh"

DD="$(mktemp -d /tmp/init-pro-builtin-image-e2e.XXXXXX)"
# kubectl is an argv[0] multicall alias (Q1) — a symlink selects it, the
# `init-pro kubectl` subcommand form does not exist.
KUBECTL="$DD/kubectl"; ln -sf "$BIN" "$KUBECTL"
SRV_PID=""; AGENT_PID=""
bimg_init_traps

API_PORT="$(pick_port)"
REG_PORT="$(pick_port)"
BASE="http://127.0.0.1:${API_PORT}"
REG="http://127.0.0.1:${REG_PORT}"
NODE="img-node"
SRV_LOG="$DD/server.log"; AGENT_LOG="$DD/agent.log"
boot_cluster

# --- I1: busybox image + explicit command ---------------------------------
IMG_DIR="$DD/img"; mkdir -p "$IMG_DIR"
cat > "$IMG_DIR/spec.json" <<'SPEC'
{"out": "IMGDIR", "layers": [[
  ["bin/busybox", "file", "@busybox", "0755"],
  ["bin/sh", "symlink", "busybox"]
]], "config": {}}
SPEC
sed -i "s#IMGDIR#$IMG_DIR#" "$IMG_DIR/spec.json"
REPO="test/builtin-img"
push_image "$IMG_DIR" "$REPO" v1 "$IMG_DIR/spec.json"
bimg_ok "micro image pushed to $REG/v2/$REPO:v1"

code="$(post_pod img-run "127.0.0.1:${REG_PORT}/$REPO:v1" '["sh","-c","echo builtin-image-ok; sleep 300"]')"
[[ "$code" == "201" ]] || bimg_fail "I1 pod POST -> $code"
PHASE="$(wait_phase img-run Running 120)"
[[ "$PHASE" == "Running" ]] \
  || bimg_fail "I1 pod never reached Running (phase=${PHASE:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
LOGS="$(wait_logs img-run builtin-image-ok 60)"
grep -q "builtin-image-ok" <<<"$LOGS" \
  || bimg_fail "I1 kubectl logs never showed the marker (got: ${LOGS:-<empty>})"
bimg_ok "I1 registry image Pod runs and logs its marker (pull->CAS->rootfs)"

# --- I2: an unknown repo must stay Pending, agent stays alive -------------
code="$(post_pod img-bad "127.0.0.1:${REG_PORT}/nope/missing:v1" '["sh","-c","echo never"]')"
[[ "$code" == "201" ]] || bimg_fail "I2 pod POST -> $code"
sleep 15
PHASE="$(pod_phase img-bad)"
[[ "$PHASE" == "Pending" ]] || bimg_fail "I2 bad-ref pod should stay Pending, got ${PHASE:-none}"
kill -0 "$AGENT_PID" 2>/dev/null || bimg_fail "I2 agent died on the pull error"
bimg_ok "I2 unknown repo stays Pending; agent survives the pull failure"

# --- I3: two layers + whiteouts -> shared per-layer unpack, overlay view --
# L1 (bottom): busybox + a.txt + del/me.txt + sub/old.txt
# L2 (top): b.txt + del/.wh.me.txt (delete) + sub/.wh..wh..opq (opaque) +
#           sub/new.txt. The Pod asserts the merged view both ways: the
#           surviving files spell "l1l2n" and the whiteouted ones are gone.
IMG2_DIR="$DD/img2"; mkdir -p "$IMG2_DIR"
cat > "$IMG2_DIR/spec.json" <<'SPEC'
{"out": "IMGDIR", "layers": [
 [["bin/busybox","file","@busybox","0755"],
  ["bin/sh","symlink","busybox"],
  ["a.txt","file","l1"],
  ["del/me.txt","file","x"],
  ["sub/old.txt","file","o"]],
 [["b.txt","file","l2"],
  ["del/.wh.me.txt","file",""],
  ["sub/.wh..wh..opq","file",""],
  ["sub/new.txt","file","n"]]
], "config": {}}
SPEC
sed -i "s#IMGDIR#$IMG2_DIR#" "$IMG2_DIR/spec.json"
REPO2="test/whtwo"
push_image "$IMG2_DIR" "$REPO2" v1 "$IMG2_DIR/spec.json"
bimg_ok "two-layer whiteout image pushed to $REG/v2/$REPO2:v1"

# post_pod's heredoc is unquoted: $3 lands verbatim in the JSON body (no
# re-expansion), so the container-side $(cat ...) reaches the Pod intact.
code="$(post_pod img-wh "127.0.0.1:${REG_PORT}/$REPO2:v1" '["sh","-c","printf %s%s%s $(cat /a.txt) $(cat /b.txt) $(cat /sub/new.txt); test ! -e /del/me.txt && test ! -e /sub/old.txt && ls /sub | grep -qx new.txt && echo WHITEOUT-OK; sleep 300"]')"
[[ "$code" == "201" ]] || bimg_fail "I3 pod POST -> $code"
PHASE="$(wait_phase img-wh Running 120)"
[[ "$PHASE" == "Running" ]] \
  || bimg_fail "I3 pod never reached Running (phase=${PHASE:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
LOGS="$(wait_logs img-wh WHITEOUT-OK 60)"
grep -q "l1l2n" <<<"$LOGS" \
  || bimg_fail "I3 merged layers missing (expected l1l2n, got: ${LOGS:-<empty>})"
grep -q "WHITEOUT-OK" <<<"$LOGS" \
  || bimg_fail "I3 whiteouts not applied (expected WHITEOUT-OK, got: ${LOGS:-<empty>})"
bimg_ok "I3 two-layer whiteout image runs ($(grep -o 'builtin image rootfs: [a-z-]* [a-z()]*' "$AGENT_LOG" | tail -1 || true))"

# --- I4: no-command Pod — the image Entrypoint IS the command (M4, Q53) --
IMG3_DIR="$DD/img3"; mkdir -p "$IMG3_DIR"
cat > "$IMG3_DIR/spec.json" <<'SPEC'
{"out": "IMGDIR", "layers": [[
  ["bin/busybox", "file", "@busybox", "0755"],
  ["bin/sh", "symlink", "busybox"],
  ["www/index.html", "file", "IMG-ENTRYPOINT", "0644"]
]], "config": {
  "Entrypoint": ["/bin/busybox", "httpd", "-f", "-v", "-p", "18080", "-h", "/www"],
  "Cmd": []}}
SPEC
sed -i "s#IMGDIR#$IMG3_DIR#" "$IMG3_DIR/spec.json"
REPO3="test/httpd"
push_image "$IMG3_DIR" "$REPO3" v1 "$IMG3_DIR/spec.json"
bimg_ok "entrypoint-only image pushed to $REG/v2/$REPO3:v1"

# NO command in the pod: the image Entrypoint must be the launch argv.
code="$(post_pod_json img-entry '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"img-entry","namespace":"default"},"spec":{"containers":[{"name":"c","image":"127.0.0.1:'"$REG_PORT"'/'"$REPO3"':v1"}]}}')"
[[ "$code" == "201" ]] || bimg_fail "I4 pod POST -> $code"
PHASE="$(wait_phase img-entry Running 120)"
[[ "$PHASE" == "Running" ]] \
  || bimg_fail "I4 no-command pod never reached Running (phase=${PHASE:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
BODY=""
for _ in $(seq 1 40); do
  BODY="$(curl -fsS http://127.0.0.1:18080/index.html 2>/dev/null || true)"
  [[ -n "$BODY" ]] && break
  sleep 0.5
done
[[ "$BODY" == "IMG-ENTRYPOINT" ]] \
  || bimg_fail "I4 curl never served the image's /www/index.html (got: ${BODY:-<empty>})"
LOGS="$(wait_logs img-entry "response:200" 60)"
grep -Eq "response:200|GET /index.html" <<<"$LOGS" \
  || bimg_fail "I4 httpd request line missing from logs (got: ${LOGS:-<empty>})"
bimg_ok "I4 no-command Pod serves via the image entrypoint (busybox httpd, Q52 host netns)"

# Free the port before the next case mounts another image. Our kubectl
# `delete` only covers the T7 CRD verbs, so the core-v1 pod goes via REST.
code="$(curl -s -o /dev/null -w '%{http_code}' -X DELETE \
  "$BASE/api/v1/namespaces/default/pods/img-entry")"
[[ "$code" == "200" || "$code" == "404" ]] || bimg_fail "I4 pod DELETE -> $code"
for _ in $(seq 1 60); do
  curl -fsS -m 1 http://127.0.0.1:18080/ >/dev/null 2>&1 || break
  sleep 0.5
done
curl -fsS -m 1 http://127.0.0.1:18080/ >/dev/null 2>&1 \
  && bimg_fail "I4 port 18080 still open after pod delete"

# --- I5: image env + pod env merge; logs + exec assertions (M4, Q53) -----
IMG4_DIR="$DD/img4"; mkdir -p "$IMG4_DIR"
cat > "$IMG4_DIR/spec.json" <<'SPEC'
{"out": "IMGDIR", "layers": [[
  ["bin/busybox", "file", "@busybox", "0755"],
  ["bin/sh", "symlink", "busybox"]
]], "config": {
  "Env": ["IMG_ENV=img-value"],
  "Entrypoint": ["/bin/sh", "-c", "echo ENVPOD-UP; sleep 300"]}}
SPEC
sed -i "s#IMGDIR#$IMG4_DIR#" "$IMG4_DIR/spec.json"
REPO4="test/envimg"
push_image "$IMG4_DIR" "$REPO4" v1 "$IMG4_DIR/spec.json"
bimg_ok "env image pushed to $REG/v2/$REPO4:v1"

# NO command; the pod ADDS an env var — it must append after the image env.
code="$(post_pod_json img-env '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"img-env","namespace":"default"},"spec":{"containers":[{"name":"c","image":"127.0.0.1:'"$REG_PORT"'/'"$REPO4"':v1","env":[{"name":"MERGED_FROM","value":"pod-value"}]}]}}')"
[[ "$code" == "201" ]] || bimg_fail "I5 pod POST -> $code"
PHASE="$(wait_phase img-env Running 120)"
[[ "$PHASE" == "Running" ]] \
  || bimg_fail "I5 no-command pod never reached Running (phase=${PHASE:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
LOGS="$(wait_logs img-env ENVPOD-UP 60)"
grep -q "ENVPOD-UP" <<<"$LOGS" \
  || bimg_fail "I5 logs never showed the entrypoint marker (got: ${LOGS:-<empty>})"
# NOTE: Ubuntu's busybox ships no `printenv` applet — `env` prints the
# whole (merged) environment, which asserts the same two facts.
EXEC1=""
for _ in $(seq 1 20); do
  EXEC1="$("$KUBECTL" exec img-env --server "$BASE" -- /bin/busybox env 2>/dev/null || true)"
  grep -q "IMG_ENV=img-value" <<<"$EXEC1" && break
  sleep 0.5
done
grep -q "IMG_ENV=img-value" <<<"$EXEC1" \
  || bimg_fail "I5 exec env never showed the image's IMG_ENV (got: ${EXEC1:-<empty>})"
EXEC2="$("$KUBECTL" exec img-env --server "$BASE" -- /bin/busybox env 2>/dev/null || true)"
grep -q "MERGED_FROM=pod-value" <<<"$EXEC2" \
  || bimg_fail "I5 exec env never showed the pod's MERGED_FROM (got: ${EXEC2:-<empty>})"
bimg_ok "I5 image env survives, pod env appends (logs + kubectl exec env)"

echo
echo "builtin-image-e2e: I1-I5 passed (real registry-v2 pull chain, T4.13 M4 image-config launch)"
