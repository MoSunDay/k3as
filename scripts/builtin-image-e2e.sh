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
#      Running, `curl 127.0.0.1:$HTTPD_PORT` returns the image's
#      index.html, `kubectl logs` shows the httpd request line (-v), and
#      the port frees after pod delete. The port is DYNAMIC (Q52 host
#      netns: any fixed port can collide with an unrelated host listener).
#      Pod deleted afterwards (port freed).
#   I5 image env vs pod env (M4): image Env IMG_ENV + Entrypoint echoing
#      ENVPOD-UP; pod adds MERGED_FROM with NO command. Asserts logs
#      show the marker and `kubectl exec ... env` sees BOTH vars
#      (image env survived, pod env appended last-wins).
#   I6 NodePort (M5, Q41/Q52): Pod img-svc (test/httpd:v1, containerPort
#      $HTTPD_PORT) + Service (port 80 -> targetPort $HTTPD_PORT); the
#      router's nodePort plane must serve the pod through the
#      auto-allocated nodePort. Then the host-port conflict rule: a second
#      pod on the same port never reaches Running (host-netns bind fails)
#      while the first pod keeps serving.
#   I7 GC (M5, Q53): test/shared:v1 embeds whtwo's layer[0] BYTES (same
#      digest); deleting whtwo's ref FILE (white-box: the ref is the unit
#      of ownership, startup-GC is v1's delete face) and restarting the
#      agent sweeps whtwo's unique layer while the SHARED base survives
#      and the in-use shared pod recovers.
#   I8 (M5): (a) CAS persistence — the registry's copy of test/httpd:v1's
#      layer blob is DELETED, a fresh pod still runs (a re-pull would
#      404); (b) --builtin-image-quota 1024 — a >4KiB image pull fails
#      cleanly (pod Pending, agent alive, quota error logged).
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
# Q52: builtin containers share the HOST netns, so the I4 httpd bind is
# global — any fixed port may collide with an unrelated host listener.
HTTPD_PORT="$(pick_port)"
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
# M5 white-box handles: whtwo's layer digests (layer[1] is whtwo-unique).
WHTWO_L0="${BIMG_LAYER_DIGESTS[0]}"; WHTWO_L1="${BIMG_LAYER_DIGESTS[1]}"

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
  "Entrypoint": ["/bin/busybox", "httpd", "-f", "-v", "-p", "HTTPDPORT", "-h", "/www"],
  "Cmd": []}}
SPEC
sed -i -e "s#IMGDIR#$IMG3_DIR#" -e "s#HTTPDPORT#$HTTPD_PORT#" "$IMG3_DIR/spec.json"
REPO3="test/httpd"
push_image "$IMG3_DIR" "$REPO3" v1 "$IMG3_DIR/spec.json"
bimg_ok "entrypoint-only image pushed to $REG/v2/$REPO3:v1"
HTTPD_LAYER="${BIMG_LAYER_DIGESTS[0]}" # M5 I8a: the registry blob to delete

# NO command in the pod: the image Entrypoint must be the launch argv.
code="$(post_pod_json img-entry '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"img-entry","namespace":"default"},"spec":{"containers":[{"name":"c","image":"127.0.0.1:'"$REG_PORT"'/'"$REPO3"':v1"}]}}')"
[[ "$code" == "201" ]] || bimg_fail "I4 pod POST -> $code"
PHASE="$(wait_phase img-entry Running 120)"
[[ "$PHASE" == "Running" ]] \
  || bimg_fail "I4 no-command pod never reached Running (phase=${PHASE:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
BODY=""
for _ in $(seq 1 40); do
  BODY="$(curl -fsS "http://127.0.0.1:${HTTPD_PORT}/index.html" 2>/dev/null || true)"
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
  curl -fsS -m 1 "http://127.0.0.1:${HTTPD_PORT}/" >/dev/null 2>&1 || break
  sleep 0.5
done
curl -fsS -m 1 "http://127.0.0.1:${HTTPD_PORT}/" >/dev/null 2>&1 \
  && bimg_fail "I4 port $HTTPD_PORT still open after pod delete"

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

# --- I6: NodePort through the router plane + host-port conflict (M5) -------
code="$(post_pod_json img-svc '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"img-svc","namespace":"default","labels":{"app":"img-svc"}},"spec":{"containers":[{"name":"c","image":"127.0.0.1:'"$REG_PORT"'/test/httpd:v1","ports":[{"name":"http","containerPort":'"$HTTPD_PORT"'}]}]}}')"
[[ "$code" == "201" ]] || bimg_fail "I6 pod POST -> $code"
code="$(api_post_json api/v1/namespaces/default/services '{"apiVersion":"v1","kind":"Service","metadata":{"name":"img-svc","namespace":"default"},"spec":{"type":"NodePort","selector":{"app":"img-svc"},"ports":[{"name":"web","port":80,"targetPort":'"$HTTPD_PORT"',"protocol":"TCP"}]}}')"
[[ "$code" == "201" ]] || bimg_fail "I6 service POST -> $code"
PHASE="$(wait_phase img-svc Running 120)"
[[ "$PHASE" == "Running" ]] \
  || bimg_fail "I6 pod never reached Running (phase=${PHASE:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
NODEPORT=""
for _ in $(seq 1 60); do
  NODEPORT="$(svc_node_port img-svc web)"
  [[ "$NODEPORT" =~ ^[0-9]+$ ]] && break
  sleep 0.5
done
[[ "$NODEPORT" =~ ^[0-9]+$ ]] || bimg_fail "I6 no auto-allocated nodePort (got '${NODEPORT:-}')"
BODY=""
for _ in $(seq 1 60); do
  BODY="$(curl -fsS "http://127.0.0.1:${NODEPORT}/index.html" 2>/dev/null || true)"
  [[ "$BODY" == "IMG-ENTRYPOINT" ]] && break
  sleep 0.5
done
[[ "$BODY" == "IMG-ENTRYPOINT" ]] \
  || bimg_fail "I6 nodePort :${NODEPORT} never served the pod (got: ${BODY:-<empty>})"
bimg_ok "I6 NodePort ${NODEPORT} -> 127.0.0.1:${HTTPD_PORT} pod (Q41 router, Q52 host netns)"

# Host-port conflict (Q52: builtin pods share the host netns): a second
# pod on ${HTTPD_PORT} must crash-loop (restartCount climbs) while the first serves.
code="$(post_pod_json img-svc2 '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"img-svc2","namespace":"default","labels":{"app":"img-svc2"}},"spec":{"containers":[{"name":"c","image":"127.0.0.1:'"$REG_PORT"'/test/httpd:v1","ports":[{"name":"http","containerPort":'"$HTTPD_PORT"'}]}]}}')"
[[ "$code" == "201" ]] || bimg_fail "I6 conflicting pod POST -> $code"
# restartPolicy Always keeps phase flickering Running<->Pending while the
# bind fails in ~50ms (status writes lag under load — phase stability is
# NOT evidence); the crash-loop VERDICT is restartCount growth (>=2).
RC2=""
for _ in $(seq 1 60); do # 30s
  RC2="$(pod_restart_count img-svc2)"
  [[ "$RC2" =~ ^[0-9]+$ ]] && [[ "$RC2" -ge 2 ]] && break
  sleep 0.5
done
[[ "${RC2:-x}" =~ ^[0-9]+$ ]] && [[ "$RC2" -ge 2 ]] \
  || bimg_fail "I6 conflicting pod never crash-looped (restartCount=${RC2:-none}; agent log: $(tail -3 "$AGENT_LOG" | tr '\n' ' '))"
BODY="$(curl -fsS "http://127.0.0.1:${NODEPORT}/index.html" 2>/dev/null || true)"
[[ "$BODY" == "IMG-ENTRYPOINT" ]] || bimg_fail "I6 first pod stopped serving after the conflict"
bimg_ok "I6 host-port conflict: second pod crash-loops, first pod keeps serving"
delete_pod img-svc2; delete_pod img-svc
code="$(api_delete api/v1/namespaces/default/services/img-svc)"
[[ "$code" == "200" || "$code" == "404" ]] || bimg_fail "I6 service DELETE -> $code"
wait_port_free "$HTTPD_PORT"
bimg_ok "I6 cleanup: pods + service gone, port $HTTPD_PORT freed"

# --- I7: startup GC sweeps unreferenced layers; shared layers survive ------
# test/shared:v1 layer[0] is byte-identical to whtwo's layer[0] (same busybox
# base); layer[1] adds s.txt. White-box (v1's delete face is startup-GC):
# the ref FILE under <agent>/engine/images/refs/ is the unit of ownership.
IMG5_DIR="$DD/img5"; mkdir -p "$IMG5_DIR"
cat > "$IMG5_DIR/spec.json" <<'SPEC'
{"out": "IMGDIR", "layers": [
 [["bin/busybox","file","@busybox","0755"],
  ["bin/sh","symlink","busybox"],
  ["a.txt","file","l1"],
  ["del/me.txt","file","x"],
  ["sub/old.txt","file","o"]],
 [["s.txt","file","s"]]
], "config": {}}
SPEC
sed -i "s#IMGDIR#$IMG5_DIR#" "$IMG5_DIR/spec.json"
REPO5="test/shared"
push_image "$IMG5_DIR" "$REPO5" v1 "$IMG5_DIR/spec.json"
[[ "${BIMG_LAYER_DIGESTS[0]}" == "$WHTWO_L0" ]] \
  || bimg_fail "I7 shared layer[0] digest mismatch (${BIMG_LAYER_DIGESTS[0]} != $WHTWO_L0)"
bimg_ok "shared image pushed; layer[0] digest == whtwo's base ($WHTWO_L0)"

code="$(post_pod img-shared "127.0.0.1:${REG_PORT}/$REPO5:v1" '["sh","-c","echo SHARED-UP; sleep 600"]')"
[[ "$code" == "201" ]] || bimg_fail "I7 pod POST -> $code"
PHASE="$(wait_phase img-shared Running 120)"
[[ "$PHASE" == "Running" ]] || bimg_fail "I7 shared pod never reached Running"
delete_pod img-wh # whtwo must be out of use before its ref goes
REFS="$DD/agent/engine/images/refs"
WHTWO_REF="$(grep -l "\"reference\":\"127.0.0.1:${REG_PORT}/test/whtwo:v1\"" "$REFS"/*.json 2>/dev/null | head -1)"
[[ -n "$WHTWO_REF" ]] || bimg_fail "I7 whtwo ref file not found under $REFS"
rm -f "$WHTWO_REF"
bimg_ok "I7 whtwo ref file deleted (${WHTWO_REF##*/}); restarting agent"
restart_agent
PHASE="$(wait_phase img-shared Running 120)"
[[ "$PHASE" == "Running" ]] \
  || bimg_fail "I7 shared pod did not recover after restart (phase=${PHASE:-none})"
L1HEX="${WHTWO_L1#sha256:}"; L0HEX="${WHTWO_L0#sha256:}"
[[ ! -e "$DD/agent/engine/images/layers/$L1HEX" && ! -e "$DD/agent/engine/images/blobs/sha256/$L1HEX" ]] \
  || bimg_fail "I7 whtwo-unique layer survived GC (layers/ or blobs/ still has $L1HEX)"
[[ -d "$DD/agent/engine/images/layers/$L0HEX" || -e "$DD/agent/engine/images/blobs/sha256/$L0HEX" ]] \
  || bimg_fail "I7 SHARED base layer was swept ($L0HEX gone)"
grep -q "builtin image GC" "$AGENT_LOG" \
  || bimg_fail "I7 startup GC never logged (agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
bimg_ok "I7 startup GC swept whtwo's unique layer; shared base + in-use pod survived"

# --- I8a: CAS persistence across restart (no re-pull) ----------------------
code="$(delete_registry_blob test/httpd "$HTTPD_LAYER")"
[[ "$code" == "202" ]] || bimg_fail "I8a registry blob DELETE -> $code (expected 202)"
code="$(post_pod_json img-cas '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"img-cas","namespace":"default"},"spec":{"containers":[{"name":"c","image":"127.0.0.1:'"$REG_PORT"'/test/httpd:v1"}]}}')"
[[ "$code" == "201" ]] || bimg_fail "I8a pod POST -> $code"
PHASE="$(wait_phase img-cas Running 120)"
[[ "$PHASE" == "Running" ]] \
  || bimg_fail "I8a pod never ran from the CAS (phase=${PHASE:-none}; a re-pull would 404 the deleted layer)"
BODY=""
for _ in $(seq 1 40); do
  BODY="$(curl -fsS "http://127.0.0.1:${HTTPD_PORT}/index.html" 2>/dev/null || true)"
  [[ -n "$BODY" ]] && break
  sleep 0.5
done
[[ "$BODY" == "IMG-ENTRYPOINT" ]] || bimg_fail "I8a CAS pod never served (got: ${BODY:-<empty>})"
bimg_ok "I8a CAS hit: pod ran with the registry blob deleted (no re-pull)"
delete_pod img-cas; wait_port_free "$HTTPD_PORT"

# --- I8b: quota soft cap -> clean pull failure, agent survives -------------
delete_pod img-bad # keep the quota error attributable to img-big
restart_agent --builtin-image-quota 1024
IMG6_DIR="$DD/img6"; mkdir -p "$IMG6_DIR"
PAD="$(printf 'P%.0s' $(seq 1 5000))"
cat > "$IMG6_DIR/spec.json" <<SPEC
{"out": "$IMG6_DIR", "layers": [[
  ["bin/busybox", "file", "@busybox", "0755"],
  ["bin/sh", "symlink", "busybox"],
  ["pad.bin", "file", "$PAD", "0644"]
]], "config": {}}
SPEC
REPO6="test/big"
push_image "$IMG6_DIR" "$REPO6" v1 "$IMG6_DIR/spec.json"
code="$(post_pod img-big "127.0.0.1:${REG_PORT}/$REPO6:v1" '["sh","-c","sleep 600"]')"
[[ "$code" == "201" ]] || bimg_fail "I8b pod POST -> $code"
QUOTA_ERR=""
for _ in $(seq 1 40); do # 20s: pod stays Pending AND the error is logged
  QUOTA_ERR="$(grep -o "builtin image store quota exceeded[^\"]*" "$AGENT_LOG" | head -1 || true)"
  PHASE="$(pod_phase img-big)"
  [[ -n "$QUOTA_ERR" && "$PHASE" != "Running" ]] && break
  sleep 0.5
done
[[ -n "$QUOTA_ERR" ]] \
  || bimg_fail "I8b quota error never logged (agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
for _ in $(seq 1 30); do # stay Pending >= 15s more
  [[ "$(pod_phase img-big)" != "Running" ]] || bimg_fail "I8b quota'd pod reached Running"
  sleep 0.5
done
kill -0 "$AGENT_PID" 2>/dev/null || bimg_fail "I8b agent died on the quota error"
[[ "$(pod_phase img-big)" != "Running" ]] || bimg_fail "I8b quota'd pod reached Running"
bimg_ok "I8b quota: pull failed cleanly ($QUOTA_ERR); pod Pending, agent alive"

echo
echo "builtin-image-e2e: I1-I8 passed (real registry-v2 pull chain, T4.13 M4 launch + M5 GC/quota)"
