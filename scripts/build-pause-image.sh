#!/usr/bin/env bash
# Deterministic, fully-offline OCI image builder for the T4.2 golden gate
# (G25) and later local-deploy scripts (decision Q27).
#
# Registry egress is blocked in this environment (registry.k8s.io redirects to
# a blocked CDN, docker.io is blocked, ghcr.io has no usable pause repo), so
# the test workload image is assembled locally from a static gcc-compiled
# pause binary and shipped as an OCI layout tar for `ctr images import`.
#
# Usage: build-pause-image.sh <out.tar> [image-ref]
#   image-ref defaults to init-pro.local/pause:0.1
#
# The output tar embeds an OCI image layout (oci-layout / index.json /
# blobs/sha256/*) with fixed mtimes, zero owners and sorted entries, so two
# runs produce byte-identical archives. No network access anywhere.
# Requires: bash, cc (or CC override), tar (GNU), gzip, python3 (hashing —
# jq/skopeo/docker are NOT assumed). Prints the image ref as the last stdout
# line on success.
set -euo pipefail

usage() { echo "usage: $(basename "$0") <out.tar> [image-ref]" >&2; exit 2; }
[[ $# -ge 1 && $# -le 2 ]] || usage
OUT_TAR="$1"
IMAGE_REF="${2:-init-pro.local/pause:0.1}"

command -v python3 >/dev/null || { echo "error: python3 not found" >&2; exit 1; }
command -v gzip    >/dev/null || { echo "error: gzip not found" >&2; exit 1; }
command -v tar     >/dev/null || { echo "error: tar not found" >&2; exit 1; }

CC="${CC:-cc}"
command -v "$CC" >/dev/null || { echo "error: C compiler '$CC' not found (set CC=...)" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

sha256_of() { python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"; }
size_of()   { python3 -c 'import os,sys;print(os.path.getsize(sys.argv[1]))' "$1"; }

# --- (a) static pause binary ------------------------------------------------
cat > "$WORK/pause.c" <<'EOF'
#include <unistd.h>
int main(void){ for(;;) pause(); }
EOF
(cd "$WORK" && "$CC" -static -Os -o pause pause.c)
# Tolerant sanity check: file/readelf may or may not exist; only hard-require
# an executable, non-empty binary.
[[ -x "$WORK/pause" && -s "$WORK/pause" ]] || { echo "error: pause binary not built" >&2; exit 1; }
if command -v file >/dev/null && file "$WORK/pause" | grep -qi 'statically linked'; then
  echo "pause: static ELF confirmed by file(1)"
fi

# --- (b) deterministic layer tar (./pause, mode 0755, owner 0:0, mtime @0) --
ROOTDIR="$WORK/rootdir"
mkdir -p "$ROOTDIR"
install -m 0755 "$WORK/pause" "$ROOTDIR/pause"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
    -cf "$WORK/layer.tar" -C "$ROOTDIR" .

DIFF_ID="$(sha256_of "$WORK/layer.tar")"

# --- (c) deterministic gzip layer blob (-n omits name/timestamp) ------------
gzip -n -9 -c "$WORK/layer.tar" > "$WORK/layer.tgz"
LAYER_DIGEST="$(sha256_of "$WORK/layer.tgz")"
LAYER_SIZE="$(size_of "$WORK/layer.tgz")"

# --- (d) OCI image layout ----------------------------------------------------
LAYOUT="$WORK/layout"
mkdir -p "$LAYOUT/blobs/sha256"
printf '{"imageLayoutVersion":"1.0.0"}' > "$LAYOUT/oci-layout"

cat > "$WORK/config.json" <<EOF
{"architecture":"amd64","os":"linux","created":"1970-01-01T00:00:00Z","config":{"Entrypoint":["/pause"],"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],"User":"0:0","Labels":{"io.init-pro.built-by":"build-pause-image.sh"}},"rootfs":{"type":"layers","diff_ids":["sha256:$DIFF_ID"]},"history":[{"created":"1970-01-01T00:00:00Z","created_by":"init-pro airgap pause"}]}
EOF
CONFIG_DIGEST="$(sha256_of "$WORK/config.json")"
CONFIG_SIZE="$(size_of "$WORK/config.json")"
cp "$WORK/config.json" "$LAYOUT/blobs/sha256/$CONFIG_DIGEST"
cp "$WORK/layer.tgz"   "$LAYOUT/blobs/sha256/$LAYER_DIGEST"

cat > "$WORK/manifest.json" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:$CONFIG_DIGEST","size":$CONFIG_SIZE},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:$LAYER_DIGEST","size":$LAYER_SIZE}],"annotations":{"org.opencontainers.image.ref.name":"$IMAGE_REF"}}
EOF
MANIFEST_DIGEST="$(sha256_of "$WORK/manifest.json")"
MANIFEST_SIZE="$(size_of "$WORK/manifest.json")"
cp "$WORK/manifest.json" "$LAYOUT/blobs/sha256/$MANIFEST_DIGEST"

cat > "$LAYOUT/index.json" <<EOF
{"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:$MANIFEST_DIGEST","size":$MANIFEST_SIZE,"annotations":{"org.opencontainers.image.ref.name":"$IMAGE_REF"}}]}
EOF

# --- (e) archive the layout (same determinism flags) -------------------------
OUT_DIR="$(cd "$(dirname "$OUT_TAR")" && pwd)"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
    -cf "$OUT_DIR/$(basename "$OUT_TAR")" -C "$LAYOUT" .

# --- (f) self-verify: re-extract and check digests/structure -----------------
SCRATCH="$WORK/scratch"
mkdir -p "$SCRATCH"
tar -xf "$OUT_DIR/$(basename "$OUT_TAR")" -C "$SCRATCH"
python3 - "$SCRATCH" <<'PY'
import hashlib, json, os, sys
root = sys.argv[1]
def die(msg):
    print(f"self-verify FAILED: {msg}", file=sys.stderr); sys.exit(1)
if open(os.path.join(root, "oci-layout")).read().strip() != '{"imageLayoutVersion":"1.0.0"}':
    die("oci-layout missing or wrong")
idx = json.load(open(os.path.join(root, "index.json")))
m = idx["manifests"][0]
blob = os.path.join(root, "blobs", "sha256", m["digest"].split(":")[1])
if not os.path.isfile(blob):
    die(f"manifest blob missing: {m['digest']}")
data = open(blob, "rb").read()
if hashlib.sha256(data).hexdigest() != m["digest"].split(":")[1]:
    die("manifest blob digest mismatch vs index.json")
if len(data) != m["size"]:
    die("manifest blob size mismatch vs index.json")
man = json.load(open(blob))
for ref in ([man["config"]] + man["layers"]):
    b = os.path.join(root, "blobs", "sha256", ref["digest"].split(":")[1])
    d = open(b, "rb").read()
    if hashlib.sha256(d).hexdigest() != ref["digest"].split(":")[1] or len(d) != ref["size"]:
        die(f"blob digest/size mismatch: {ref['digest']}")
print("self-verify: OCI layout, index, manifest, config and layer digests all consistent")
PY

echo "$IMAGE_REF"
