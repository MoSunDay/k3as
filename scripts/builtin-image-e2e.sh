#!/usr/bin/env bash
# T4.13 (Q53/Q54): end-to-end exercise of the REAL image pull chain —
# a docker-v2 image pushed to the server's embedded registry is pulled by
# the builtin engine's kubelet (manifest verify -> CAS blobs -> shared
# per-layer unpack) and RUNS as a Pod.
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
push_blob() { # REPO FILE DIGEST -> echoes the POST status (201 = ok)
  curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H 'Content-Type: application/octet-stream' \
    --data-binary "@$2" "$REG/v2/$1/blobs/uploads/?digest=$3"
}
for f_d in "$IMG_DIR/config.json:$CFG_D" "$IMG_DIR/layer.tar.gz:$LAY_D"; do
  f="${f_d%%:*}"; d="${f_d#*:}"
  code="$(push_blob "$REPO" "$f" "$d")"
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
ok "I1 registry image Pod runs and logs its marker (pull->CAS->rootfs)"

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

# --- I3: two layers + whiteouts -> shared per-layer unpack, overlay view --
# L1 (bottom): busybox + a.txt + del/me.txt + sub/old.txt
# L2 (top): b.txt + del/.wh.me.txt (delete) + sub/.wh..wh..opq (opaque) +
#           sub/new.txt. The Pod asserts the merged view both ways: the
#           surviving files spell "l1l2n" and the whiteouted ones are gone.
pod_phase() { # NAME -> current phase ("" on error)
  curl -s "$BASE/api/v1/namespaces/default/pods/$1" | python3 -c '
import json,sys
try: print(json.load(sys.stdin)["status"].get("phase",""))
except Exception: print("")
' 2>/dev/null || true
}
IMG2_DIR="$DD/img2"; mkdir -p "$IMG2_DIR"
read -r C2_D C2_S W1_D W1_S W2_D W2_S < <(python3 - "$BUSYBOX" "$IMG2_DIR" <<'PY'
import gzip, hashlib, io, json, os, sys, tarfile

busybox, out = sys.argv[1], sys.argv[2]

def sha256_bytes(b):
    return "sha256:" + hashlib.sha256(b).hexdigest()

with open(busybox, "rb") as f:
    bb = f.read()

l1_entries = [
    ("bin/busybox", "file", bb),
    ("bin/sh", "symlink", "busybox"),
    ("a.txt", "file", b"l1"),
    ("del/me.txt", "file", b"x"),
    ("sub/old.txt", "file", b"o"),
]
l2_entries = [
    ("b.txt", "file", b"l2"),
    ("del/.wh.me.txt", "file", b""),
    ("sub/.wh..wh..opq", "file", b""),
    ("sub/new.txt", "file", b"n"),
]
layers = []
diff_ids = []
for name, entries in (("l1", l1_entries), ("l2", l2_entries)):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as tf:
        for path, kind, data in entries:
            ti = tarfile.TarInfo(path)
            if kind == "symlink":
                ti.type = tarfile.SYMTYPE
                ti.linkname = data
                ti.mode = 0o777
            else:
                ti.size = len(data)
                ti.mode = 0o755 if path == "bin/busybox" else 0o644
            tf.addfile(ti, io.BytesIO(data) if kind == "file" else None)
    tar_raw = raw.getvalue()
    blob = gzip.compress(tar_raw, mtime=0)
    with open(os.path.join(out, name + ".tar.gz"), "wb") as f:
        f.write(blob)
    layers.append((sha256_bytes(blob), len(blob)))
    diff_ids.append(sha256_bytes(tar_raw))

cfg = {"architecture": "amd64", "os": "linux",
       "rootfs": {"type": "layers", "diff_ids": diff_ids}, "history": []}
cfg_b = json.dumps(cfg, separators=(",", ":")).encode()
cfg_p = os.path.join(out, "config.json")
with open(cfg_p, "wb") as f:
    f.write(cfg_b)
print(sha256_bytes(cfg_b), len(cfg_b), layers[0][0], layers[0][1],
      layers[1][0], layers[1][1])
PY
)
REPO2="test/whtwo"
for f_d in "$IMG2_DIR/config.json:$C2_D" "$IMG2_DIR/l1.tar.gz:$W1_D" "$IMG2_DIR/l2.tar.gz:$W2_D"; do
  f="${f_d%%:*}"; d="${f_d#*:}"
  code="$(push_blob "$REPO2" "$f" "$d")"
  [[ "$code" == "201" ]] || fail "I3 blob POST $d -> $code (expected 201)"
done
MAN2="$IMG2_DIR/manifest.json"
python3 - "$MAN2" "$C2_D" "$C2_S" "$W1_D" "$W1_S" "$W2_D" "$W2_S" <<'PY'
import json, sys
out = sys.argv[1]
c_d, c_s, l1_d, l1_s, l2_d, l2_s = sys.argv[2:8]
manifest = {
    "schemaVersion": 2,
    "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
    "config": {"mediaType": "application/vnd.docker.container.image.v1+json",
               "size": int(c_s), "digest": c_d},
    "layers": [
        {"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
         "size": int(l1_s), "digest": l1_d},
        {"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
         "size": int(l2_s), "digest": l2_d},
    ],
}
with open(out, "w") as f:
    json.dump(manifest, f, separators=(",", ":"))
PY
code="$(curl -s -o "$DD/put2.out" -w '%{http_code}' -X PUT \
  -H 'Content-Type: application/vnd.docker.distribution.manifest.v2+json' \
  --data-binary "@$MAN2" "$REG/v2/$REPO2/manifests/v1")"
[[ "$code" == "201" ]] || fail "I3 manifest PUT v1 -> $code (expected 201): $(head -c 200 "$DD/put2.out")"
ok "two-layer whiteout image pushed to $REG/v2/$REPO2:v1"

# post_pod's heredoc is unquoted: $3 lands verbatim in the JSON body (no
# re-expansion), so the container-side $(cat ...) reaches the Pod intact.
code="$(post_pod img-wh "127.0.0.1:${REG_PORT}/$REPO2:v1" '["sh","-c","printf %s%s%s $(cat /a.txt) $(cat /b.txt) $(cat /sub/new.txt); test ! -e /del/me.txt && test ! -e /sub/old.txt && ls /sub | grep -qx new.txt && echo WHITEOUT-OK; sleep 300"]')"
[[ "$code" == "201" ]] || fail "I3 pod POST -> $code"
PHASE=""
for _ in $(seq 1 120); do
  PHASE="$(pod_phase img-wh)"
  [[ "$PHASE" == "Running" ]] && break
  sleep 0.5
done
[[ "$PHASE" == "Running" ]] \
  || fail "I3 pod never reached Running (phase=${PHASE:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
LOGS=""
for _ in $(seq 1 60); do
  LOGS="$("$KUBECTL" logs img-wh --server "$BASE" 2>/dev/null || true)"
  grep -q "WHITEOUT-OK" <<<"$LOGS" && break
  sleep 0.5
done
grep -q "l1l2n" <<<"$LOGS" \
  || fail "I3 merged layers missing (expected l1l2n, got: ${LOGS:-<empty>})"
grep -q "WHITEOUT-OK" <<<"$LOGS" \
  || fail "I3 whiteouts not applied (expected WHITEOUT-OK, got: ${LOGS:-<empty>})"
# Informational: which Q53 rootfs path this node took.
ROOTFS_MODE="$(grep -o 'builtin image rootfs: [a-z-]* [a-z()]*' "$AGENT_LOG" | tail -1 || true)"
ok "I3 two-layer whiteout image runs (${ROOTFS_MODE:-rootfs mode unlogged})"

echo
echo "builtin-image-e2e: I1-I3 passed (real registry-v2 pull chain, T4.13)"
