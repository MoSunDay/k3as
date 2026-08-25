#!/usr/bin/env bash
# Deterministic, fully-offline OCI image builder for the T7.3 Scope B
# golden gate (G29) and later local-deploy scripts (decision Q27).
#
# The Argo Workflow step workload must EXIT: the Q27 airgap pause/echo
# images never terminate (pause blocks forever; echo serves HTTP), while
# a Workflow step pod's exit code IS the step's verdict (restartPolicy
# Never). So this script packages `wfstep`, a tiny static one-shot binary:
#   wfstep <message>          -> prints "wfstep: <message>" to stdout, exit 0
#   wfstep --fail <message>   -> prints "wfstep: <message>" to stderr, exit 1
#
# Registry egress is blocked in this environment, so the image is
# assembled locally from a cc-compiled static binary and shipped as an OCI
# layout tar for `ctr images import` — same recipe as
# scripts/build-pause-image.sh (Q27).
#
# Usage: build-workflow-image.sh <out.tar> [image-ref]
#   image-ref defaults to init-pro.local/wfstep:0.1
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
IMAGE_REF="${2:-init-pro.local/wfstep:0.1}"

command -v python3 >/dev/null || { echo "error: python3 not found" >&2; exit 1; }
command -v gzip    >/dev/null || { echo "error: gzip not found" >&2; exit 1; }
command -v tar     >/dev/null || { echo "error: tar not found" >&2; exit 1; }

CC="${CC:-cc}"
command -v "$CC" >/dev/null || { echo "error: C compiler '$CC' not found (set CC=...)" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

sha256_of() { python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"; }
size_of()   { python3 -c 'import os,sys;print(os.path.getsize(sys.argv[1]))' "$1"; }

# --- (a) static wfstep binary ------------------------------------------------
# One-shot step workload: argv[1] == "--fail" flips the stream (stderr) and
# the exit code (1); the message is argv[2] then, else argv[1]. No libc
# beyond stdio/strcmp, no dependencies.
cat > "$WORK/wfstep.c" <<'EOF'
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    int fail = argc > 1 && strcmp(argv[1], "--fail") == 0;
    const char *msg = fail ? (argc > 2 ? argv[2] : "") : (argc > 1 ? argv[1] : "");
    if (fail) { fprintf(stderr, "wfstep: %s\n", msg); return 1; }
    printf("wfstep: %s\n", msg);
    return 0;
}
EOF
(cd "$WORK" && "$CC" -static -Os -o wfstep wfstep.c)
# Tolerant sanity check: file/readelf may or may not exist; only hard-require
# an executable, non-empty binary.
[[ -x "$WORK/wfstep" && -s "$WORK/wfstep" ]] || { echo "error: wfstep binary not built" >&2; exit 1; }
if command -v file >/dev/null && file "$WORK/wfstep" | grep -qi 'statically linked'; then
  echo "wfstep: static ELF confirmed by file(1)"
fi
# Behavior self-test on the host (static binary, no container needed): the
# success and failure contracts the Workflow steps rely on.
GOT="$("$WORK/wfstep" step-a)"
[[ "$GOT" == "wfstep: step-a" ]] || { echo "error: wfstep stdout mismatch: '$GOT'" >&2; exit 1; }
if "$WORK/wfstep" --fail boom >/dev/null 2>"$WORK/fail.err"; then
  echo "error: wfstep --fail exited 0" >&2; exit 1
fi
[[ "$(cat "$WORK/fail.err")" == "wfstep: boom" ]] || { echo "error: wfstep --fail stderr mismatch" >&2; exit 1; }
echo "wfstep: success + failure contracts verified on the host"

# --- (b) deterministic layer tar (./bin/wfstep, mode 0755, owner 0:0, mtime @0)
# /bin is on the image PATH (echo-image precedent), so both Entrypoint
# ["/bin/wfstep"] and a bare `command: [wfstep]` step resolve.
ROOTDIR="$WORK/rootdir"
mkdir -p "$ROOTDIR/bin"
install -m 0755 "$WORK/wfstep" "$ROOTDIR/bin/wfstep"
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
{"architecture":"amd64","os":"linux","created":"1970-01-01T00:00:00Z","config":{"Entrypoint":["/bin/wfstep"],"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],"User":"0:0","Labels":{"io.init-pro.built-by":"build-workflow-image.sh"}},"rootfs":{"type":"layers","diff_ids":["sha256:$DIFF_ID"]},"history":[{"created":"1970-01-01T00:00:00Z","created_by":"init-pro airgap wfstep"}]}
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

# --- (f) self-verify: re-extract, check digests + run the packaged binary ----
SCRATCH="$WORK/scratch"
mkdir -p "$SCRATCH"
tar -xf "$OUT_DIR/$(basename "$OUT_TAR")" -C "$SCRATCH"
python3 - "$SCRATCH" <<'PY'
import gzip, hashlib, json, os, sys, tarfile
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
# Unpack the layer in-memory and confirm the one-shot binary is in it.
with tarfile.open(fileobj=gzip.open(os.path.join(root, "blobs", "sha256", man["layers"][0]["digest"].split(":")[1])), mode="r|") as tf:
    names = tf.getnames()
if "bin/wfstep" not in names and "./bin/wfstep" not in names:
    die(f"bin/wfstep not in layer: {names}")
print("self-verify: OCI layout, index, manifest, config and layer digests all consistent")
PY

echo "$IMAGE_REF"
