#!/usr/bin/env bash
# T4.13 M2 (Q53/Q54): end-to-end exercise of the REAL image pull chain —
# a docker-v2 image pushed to the server's embedded registry is pulled by
# the builtin engine's kubelet (manifest verify -> CAS blobs -> fused
# extraction) and RUNS as a Pod.
#
# Cases:
#   I1 real image Pod: micro OCI image (busybox layer) pushed as
#      test/builtin-img:v1 -> Pod runs it with an explicit command ->
#      `kubectl logs` shows the marker.
#   I2 bad ref: unknown repo stays Pending (pull error must not kill the
#      agent loop).
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

DD="$(mktemp -d /tmp/init-pro-builtin-image-e2e.XXXXXX)"
# kubectl is an argv[0] multicall alias (Q1) — a symlink selects it, the
# `init-pro kubectl` subcommand form does not exist.
KUBECTL="$DD/kubectl"; ln -sf "$BIN" "$KUBECTL"
SRV_PID=""; AGENT_PID=""
cleanup() {
  [[ -n "$AGENT_PID" ]] && kill "$AGENT_PID" 2>/dev/null || true
  [[ -n "$SRV_PID" ]] && kill "$SRV_PID" 2>/dev/null || true
  [[ -n "$AGENT_PID" ]] && wait "$AGENT_PID" 2>/dev/null || true
  [[ -n "$SRV_PID" ]] && wait "$SRV_PID" 2>/dev/null || true
  rm -rf "$DD" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

fail() { echo "FAIL $*" >&2; exit 1; }
ok() { echo "OK   $*"; }

pick_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

API_PORT="$(pick_port)"
REG_PORT="$(pick_port)"
BASE="http://127.0.0.1:${API_PORT}"
REG="http://127.0.0.1:${REG_PORT}"
NODE="img-node"
SRV_LOG="$DD/server.log"; AGENT_LOG="$DD/agent.log"

# --- boot the cluster: server (embedded registry) + builtin agent ---------
"$BIN" server --data-dir "$DD/srv" --bind-address 127.0.0.1 \
  --https-listen-port "$API_PORT" \
  --registry-addr "127.0.0.1:${REG_PORT}" >"$SRV_LOG" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 100); do
  grep -q "discovery listening" "$SRV_LOG" && break; sleep 0.1
done
grep -q "discovery listening" "$SRV_LOG" \
  || fail "server never reported listening ($(tail -5 "$SRV_LOG" | tr '\n' ' '))"
for _ in $(seq 1 100); do
  [[ "$(curl -s -o /dev/null -w '%{http_code}' "$REG/v2/" 2>/dev/null)" == "200" ]] && break; sleep 0.1
done
[[ "$(curl -s -o /dev/null -w '%{http_code}' "$REG/v2/")" == "200" ]] \
  || fail "embedded registry never answered at $REG"

"$BIN" agent --data-dir "$DD/agent" --token dev --server "$BASE" \
  --node-name "$NODE" --container-engine builtin \
  --registry-addr "127.0.0.1:${REG_PORT}" >"$AGENT_LOG" 2>&1 &
AGENT_PID=$!
for _ in $(seq 1 300); do
  grep -q "kubelet started (T4.12 built-in engine)" "$AGENT_LOG" && break; sleep 0.1
done
grep -q "kubelet started (T4.12 built-in engine)" "$AGENT_LOG" \
  || fail "builtin agent never came up ($(tail -5 "$AGENT_LOG" | tr '\n' ' '))"

# --- fabricate the micro OCI image (docker v2, 1 gzip layer) --------------
IMG_DIR="$DD/img"; mkdir -p "$IMG_DIR"
# Builder prints "<cfg-digest> <cfg-size> <layer-digest> <layer-size>".
read -r CFG_D CFG_S LAY_D LAY_S < <(python3 - "$BUSYBOX" "$IMG_DIR" <<'PY'
import gzip, hashlib, io, json, os, sys, tarfile

busybox, out = sys.argv[1], sys.argv[2]

def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 16), b""):
            h.update(chunk)
    return "sha256:" + h.hexdigest()

# Layer: /bin/busybox (real static binary) + /bin/sh -> busybox symlink.
buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode="w") as tf:
    data = open(busybox, "rb").read()
    ti = tarfile.TarInfo("bin/busybox")
    ti.size = len(data)
    ti.mode = 0o755
    tf.addfile(ti, io.BytesIO(data))
    sh = tarfile.TarInfo("bin/sh")
    sh.type = tarfile.SYMTYPE
    sh.linkname = "busybox"
    sh.mode = 0o777
    tf.addfile(sh)
layer_gz = os.path.join(out, "layer.tar.gz")
with open(layer_gz, "wb") as f:
    with gzip.GzipFile(fileobj=f, mode="wb", mtime=0) as gz:
        gz.write(buf.getvalue())

config = {
    "architecture": "amd64",
    "os": "linux",
    "config": {},
    "rootfs": {"type": "layers", "diff_ids": []},  # registry does not validate
}
cfg_path = os.path.join(out, "config.json")
with open(cfg_path, "w") as f:
    json.dump(config, f, separators=(",", ":"))

cfg_d, cfg_s = sha256(cfg_path), os.path.getsize(cfg_path)
lay_d, lay_s = sha256(layer_gz), os.path.getsize(layer_gz)
manifest = {
    "schemaVersion": 2,
    "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
    "config": {
        "mediaType": "application/vnd.docker.container.image.v1+json",
        "size": cfg_s,
        "digest": cfg_d,
    },
    "layers": [
        {
            "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
            "size": lay_s,
            "digest": lay_d,
        }
    ],
}
man_path = os.path.join(out, "manifest.json")
with open(man_path, "w") as f:
    json.dump(manifest, f, separators=(",", ":"))
print(cfg_d, cfg_s, lay_d, lay_s)
PY
)

# --- push blobs + manifest (monolithic POST, registry-e2e recipe) ---------
REPO="test/builtin-img"
push_blob() { # FILE DIGEST -> echoes the POST status (201 = ok)
  curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H 'Content-Type: application/octet-stream' \
    --data-binary "@$1" "$REG/v2/$REPO/blobs/uploads/?digest=$2"
}
for f_d in "$IMG_DIR/config.json:$CFG_D" "$IMG_DIR/layer.tar.gz:$LAY_D"; do
  f="${f_d%%:*}"; d="${f_d#*:}"
  code="$(push_blob "$f" "$d")"
  [[ "$code" == "201" ]] || fail "blob POST $d -> $code (expected 201)"
done
code="$(curl -s -o "$DD/put.out" -w '%{http_code}' -X PUT \
  -H 'Content-Type: application/vnd.docker.distribution.manifest.v2+json' \
  --data-binary "@$IMG_DIR/manifest.json" "$REG/v2/$REPO/manifests/v1")"
[[ "$code" == "201" ]] || fail "manifest PUT v1 -> $code (expected 201): $(head -c 200 "$DD/put.out")"
ok "micro image pushed to $REG/v2/$REPO:v1"

# --- I1: a Pod running the pulled image with an explicit command ----------
post_pod() { # NAME IMAGE ARGS_JSON -> echoes the POST status
  curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' \
    --data-binary @- "$BASE/api/v1/namespaces/default/pods" <<EOF
{"apiVersion":"v1","kind":"Pod","metadata":{"name":"$1","namespace":"default"},
 "spec":{"containers":[{"name":"c","image":"$2","command":["/bin/busybox"],"args":$3}]}}
EOF
}
code="$(post_pod img-run "127.0.0.1:${REG_PORT}/$REPO:v1" '["sh","-c","echo builtin-image-ok; sleep 300"]')"
[[ "$code" == "201" ]] || fail "I1 pod POST -> $code"

PHASE=""
for _ in $(seq 1 120); do
  PHASE="$(curl -s "$BASE/api/v1/namespaces/default/pods/img-run" | python3 -c '
import json,sys
try: print(json.load(sys.stdin)["status"].get("phase",""))
except Exception: print("")
' 2>/dev/null || true)"
  [[ "$PHASE" == "Running" ]] && break
  sleep 0.5
done
[[ "$PHASE" == "Running" ]] \
  || fail "I1 pod never reached Running (phase=${PHASE:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"

LOGS=""
for _ in $(seq 1 60); do
  LOGS="$("$KUBECTL" logs img-run --server "$BASE" 2>/dev/null || true)"
  grep -q "builtin-image-ok" <<<"$LOGS" && break
  sleep 0.5
done
grep -q "builtin-image-ok" <<<"$LOGS" \
  || fail "I1 kubectl logs never showed the marker (got: ${LOGS:-<empty>})"
ok "I1 registry image Pod runs and logs its marker (pull->CAS->fused rootfs)"

# --- I2: an unknown repo must stay Pending, agent stays alive -------------
code="$(post_pod img-bad "127.0.0.1:${REG_PORT}/nope/missing:v1" '["sh","-c","echo never"]')"
[[ "$code" == "201" ]] || fail "I2 pod POST -> $code"
sleep 15
PHASE="$(curl -s "$BASE/api/v1/namespaces/default/pods/img-bad" | python3 -c '
import json,sys
try: print(json.load(sys.stdin)["status"].get("phase",""))
except Exception: print("")
' 2>/dev/null || true)"
[[ "$PHASE" == "Pending" ]] || fail "I2 bad-ref pod should stay Pending, got ${PHASE:-none}"
kill -0 "$AGENT_PID" 2>/dev/null || fail "I2 agent died on the pull error"
ok "I2 unknown repo stays Pending; agent survives the pull failure"

echo
echo "builtin-image-e2e: I1-I2 passed (real registry-v2 pull chain, T4.13 M2)"
