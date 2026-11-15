#!/usr/bin/env bash
# Deterministic, fully-offline OCI image builder for the cross-node flannel
# acceptance workload (host-gw / vxlan pod-to-pod checks). Mirrors the inline
# netpod recipe historically embedded in scripts/flannel-e2e.sh (Q27 recipe,
# Q34 flannel): one static C binary, three argv modes -
#   netpod serve <port>   TCP echo responder ("netpod-ok <line>")
#   netpod get <ip> <port> connect + read, prints reply (exit 0 on netpod-ok)
#   netpod idle           sleep forever (image Entrypoint)
#
# Usage: build-netpod-image.sh <out.tar> [image-ref]
#   image-ref defaults to init-pro.local/netpod:0.1
# Fully offline; deterministic tar (fixed mtimes, zero owners, sorted).
# Requires: bash, cc (or CC override), tar (GNU), gzip, python3.
set -euo pipefail

usage() { echo "usage: $(basename "$0") <out.tar> [image-ref]" >&2; exit 2; }
[[ $# -ge 1 && $# -le 2 ]] || usage
OUT_TAR="$1"
IMAGE_REF="${2:-init-pro.local/netpod:0.1}"

command -v python3 >/dev/null || { echo "error: python3 not found" >&2; exit 1; }
command -v gzip    >/dev/null || { echo "error: gzip not found" >&2; exit 1; }
command -v tar     >/dev/null || { echo "error: tar not found" >&2; exit 1; }
CC="${CC:-cc}"
command -v "$CC" >/dev/null 2>&1 || { echo "error: C compiler '$CC' not found" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

sha256_of() { python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"; }
size_of()   { python3 -c 'import os,sys;print(os.path.getsize(sys.argv[1]))' "$1"; }

mkdir -p "$WORK/src" "$WORK/rootdir"
cat > "$WORK/src/netpod.c" <<'EOF'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
static void serve(int port) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_port = htons((unsigned short)port); a.sin_addr.s_addr = INADDR_ANY;
    if (bind(s, (struct sockaddr*)&a, sizeof a) != 0) { perror("bind"); exit(1); }
    if (listen(s, 16) != 0) { perror("listen"); exit(1); }
    for (;;) {
        int c = accept(s, 0, 0);
        if (c < 0) continue;
        char buf[512]; ssize_t n = read(c, buf, sizeof buf - 1);
        if (n < 0) n = 0; buf[n > 0 ? n : 0] = 0;
        dprintf(c, "netpod-ok %s\n", buf);
        close(c);
    }
}
static int get(const char* ip, int port) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_port = htons((unsigned short)port);
    if (inet_pton(AF_INET, ip, &a.sin_addr) != 1) return 64;
    if (connect(s, (struct sockaddr*)&a, sizeof a) != 0) { perror("connect"); return 1; }
    dprintf(s, "ping\n");
    char buf[512]; ssize_t n = read(s, buf, sizeof buf - 1);
    if (n <= 0) return 2;
    buf[n] = 0; printf("%s", buf);
    return strncmp(buf, "netpod-ok", 9) == 0 ? 0 : 3;
}
int main(int argc, char** argv) {
    if (argc == 3 && !strcmp(argv[1], "serve")) { serve(atoi(argv[2])); return 0; }
    if (argc == 4 && !strcmp(argv[1], "get"))   { return get(argv[2], atoi(argv[3])); }
    if (argc == 2 && !strcmp(argv[1], "idle"))  { for (;;) pause(); }
    fprintf(stderr, "usage: netpod serve <port> | get <ip> <port> | idle\n");
    return 64;
}
EOF
(cd "$WORK/src" && "$CC" -static -Os -o "$WORK/rootdir/netpod" netpod.c)
[[ -x "$WORK/rootdir/netpod" && -s "$WORK/rootdir/netpod" ]] || { echo "error: netpod binary not built" >&2; exit 1; }

tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
    -cf "$WORK/layer.tar" -C "$WORK/rootdir" .
DIFF_ID="$(sha256_of "$WORK/layer.tar")"
gzip -n -9 -c "$WORK/layer.tar" > "$WORK/layer.tgz"
LAYER_DIGEST="$(sha256_of "$WORK/layer.tgz")"
LAYER_SIZE="$(size_of "$WORK/layer.tgz")"

LAYOUT="$WORK/layout"
mkdir -p "$LAYOUT/blobs/sha256"
printf '{"imageLayoutVersion":"1.0.0"}' > "$LAYOUT/oci-layout"

cat > "$WORK/config.json" <<EOF
{"architecture":"amd64","os":"linux","created":"1970-01-01T00:00:00Z","config":{"Entrypoint":["/netpod","idle"],"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],"User":"0:0","Labels":{"io.init-pro.built-by":"build-netpod-image.sh"}},"rootfs":{"type":"layers","diff_ids":["sha256:$DIFF_ID"]},"history":[{"created":"1970-01-01T00:00:00Z","created_by":"init-pro netpod image"}]}
EOF
CONFIG_DIGEST="$(sha256_of "$WORK/config.json")"
CONFIG_SIZE="$(size_of "$WORK/config.json")"
cp "$WORK/config.json" "$LAYOUT/blobs/sha256/$CONFIG_DIGEST"
cp "$WORK/layer.tgz" "$LAYOUT/blobs/sha256/$LAYER_DIGEST"

cat > "$WORK/manifest.json" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:$CONFIG_DIGEST","size":$CONFIG_SIZE},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:$LAYER_DIGEST","size":$LAYER_SIZE}],"annotations":{"org.opencontainers.image.ref.name":"$IMAGE_REF"}}
EOF
MANIFEST_DIGEST="$(sha256_of "$WORK/manifest.json")"
MANIFEST_SIZE="$(size_of "$WORK/manifest.json")"
cp "$WORK/manifest.json" "$LAYOUT/blobs/sha256/$MANIFEST_DIGEST"

cat > "$LAYOUT/index.json" <<EOF
{"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:$MANIFEST_DIGEST","size":$MANIFEST_SIZE,"annotations":{"org.opencontainers.image.ref.name":"$IMAGE_REF"}}]}
EOF

OUT_DIR="$(cd "$(dirname "$OUT_TAR")" && pwd)"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
    -cf "$OUT_DIR/$(basename "$OUT_TAR")" -C "$LAYOUT" .
echo "self-verify: netpod image $(sha256_of "$OUT_DIR/$(basename "$OUT_TAR")" | head -c 12)"
echo "$IMAGE_REF"
