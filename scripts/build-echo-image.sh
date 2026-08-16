#!/usr/bin/env bash
# Deterministic, fully-offline OCI image builder for Sprint 18 / S5: packages
# a static C HTTP echo server as the load-balancing test workload for the S4
# NodePort plane (Q28) and the S6 two-replica round-robin assertion.
#
# Registry egress is blocked in this environment (registry.k8s.io redirects to
# a blocked CDN, docker.io is blocked, ghcr.io has no usable echo repo), so
# the test workload image is assembled locally from a static gcc-compiled
# echo binary and shipped as an OCI layout tar for `ctr images import` —
# same recipe as scripts/build-pause-image.sh (Q27).
#
# Usage: build-echo-image.sh <out.tar> [image-ref]
#   image-ref defaults to init-pro.local/echo:0.1
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
IMAGE_REF="${2:-init-pro.local/echo:0.1}"

command -v python3 >/dev/null || { echo "error: python3 not found" >&2; exit 1; }
command -v gzip    >/dev/null || { echo "error: gzip not found" >&2; exit 1; }
command -v tar     >/dev/null || { echo "error: tar not found" >&2; exit 1; }

CC="${CC:-cc}"
command -v "$CC" >/dev/null || { echo "error: C compiler '$CC' not found (set CC=...)" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

sha256_of() { python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"; }
size_of()   { python3 -c 'import os,sys;print(os.path.getsize(sys.argv[1]))' "$1"; }

# --- (a) static echo binary --------------------------------------------------
# Minimal HTTP/1.1 echo server: one fact per line (ECHO/LOCAL/METHOD/PATH/
# HEADER*/BODY), Connection: close per request, SIGPIPE ignored, malformed
# input just
# drops the connection. Port: $PORT env, else argv[1], else 8080. No threads:
# a sequential accept loop is plenty for e2e traffic. LOCAL is the accepted
# socket's local address (podIP) — the unique per-replica discriminator for
# the S6 round-robin assert, since pod hostnames are not plumbed through CRI
# yet (kubelet sends no PodSandboxConfig.hostname).
cat > "$WORK/echo.c" <<'EOF'
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/socket.h>
#include <unistd.h>

#define HEAD_CAP (64 * 1024)   /* whole header block cap */
#define BODY_CAP (1024 * 1024) /* Content-Length body cap (1 MiB) */

static char head[HEAD_CAP]; /* single-threaded: one static buffer is fine */

/* recv() that retries EINTR; returns bytes read, 0 on EOF, -1 on error. */
static ssize_t read_some(int fd, void *buf, size_t n) {
  for (;;) {
    ssize_t r = recv(fd, buf, n, 0);
    if (r >= 0 || errno != EINTR) return r;
  }
}

static int write_all(int fd, const char *p, size_t n) {
  while (n > 0) {
    ssize_t w = send(fd, p, n, 0);
    if (w < 0) {
      if (errno == EINTR) continue;
      return -1; /* peer gone (EPIPE etc.): SIGPIPE ignored, just drop */
    }
    p += w;
    n -= (size_t)w;
  }
  return 0;
}

/* Offset of the "\r" starting the end-of-headers "\r\n\r\n", or -1. */
static long headers_end(const char *buf, size_t n) {
  for (size_t i = 0; i + 3 < n; i++)
    if (memcmp(buf + i, "\r\n\r\n", 4) == 0) return (long)i;
  return -1;
}

static void handle(int cfd) {
  size_t got = 0;
  long hend = -1;
  while (got < HEAD_CAP - 1) { /* tolerate partial reads: loop recv */
    ssize_t r = read_some(cfd, head + got, HEAD_CAP - 1 - got);
    if (r <= 0) break;
    got += (size_t)r;
    hend = headers_end(head, got);
    if (hend >= 0) break;
  }
  if (hend < 0) return; /* EOF/error/oversized before headers done: drop */
  size_t body_off = (size_t)hend + 4; /* first body byte */

  char *line_end = memchr(head, '\r', body_off);
  if (!line_end) return; /* cannot happen (hend found), stay paranoid */
  *line_end = '\0';
  char *method = head, *target = "-";
  char *sp1 = strchr(method, ' ');
  if (sp1) { /* request line: METHOD SP target SP version */
    *sp1 = '\0';
    target = sp1 + 1;
    char *sp2 = strchr(target, ' ');
    if (sp2) *sp2 = '\0';
  } else {
    method = "-"; /* malformed request line: still answer, never crash */
  }

  char *hlines[256];
  int nh = 0;
  long clen = -1;
  char *stop = head + hend; /* "\r" ending the last header line */
  for (char *p = line_end + 2; p < stop && nh < 256;) {
    char *cr = memchr(p, '\r', (size_t)(stop + 1 - p));
    if (!cr) break;
    *cr = '\0';
    if (*p) {
      hlines[nh++] = p;
      if (strncasecmp(p, "content-length:", 15) == 0) {
        char *v = p + 15;
        while (*v == ' ' || *v == '\t') v++;
        clen = strtol(v, NULL, 10); /* absent/unparsable -> clen <= 0: no body */
      }
    }
    p = cr + 2;
  }

  char *body = NULL;
  size_t body_len = 0;
  if (clen > 0) { /* no Content-Length -> no body read */
    size_t want = (size_t)clen;
    if (want > BODY_CAP) want = BODY_CAP; /* cap; the rest is ignored */
    body = malloc(want + 1);
    if (!body) return;
    size_t buffered = got > body_off ? got - body_off : 0;
    size_t take = buffered < want ? buffered : want;
    memcpy(body, head + body_off, take);
    body_len = take;
    while (body_len < want) {
      ssize_t r = read_some(cfd, body + body_len, want - body_len);
      if (r <= 0) break; /* short body: echo what arrived */
      body_len += (size_t)r;
    }
    body[body_len] = '\0';
  }

  char host[256];
  host[0] = '\0';
  gethostname(host, sizeof(host) - 1);
  /* kubelet does not plumb PodSandboxConfig.hostname yet, so every pod
   * inherits the node hostname; the accepted socket's local address is the
   * unique per-replica discriminator (podIP) for the S6 round-robin assert */
  char local[64] = "?";
  struct sockaddr_in la;
  socklen_t lalen = sizeof(la);
  if (getsockname(cfd, (struct sockaddr *)&la, &lalen) == 0)
    inet_ntop(AF_INET, &la.sin_addr, local, sizeof(local));

  size_t cap = 512 + 3 * body_off + body_len + 16;
  char *resp = malloc(cap);
  if (!resp) { free(body); return; }
  int n = snprintf(resp, cap, "ECHO %s\nLOCAL %s\nMETHOD %s\nPATH %s\n",
                   host, local, method, target);
  for (int i = 0; i < nh && n > 0 && (size_t)n < cap; i++)
    n += snprintf(resp + n, cap - (size_t)n, "HEADER %s\n", hlines[i]);
  if (body_len > 0 && n > 0 && (size_t)n < cap) {
    n += snprintf(resp + n, cap - (size_t)n, "BODY ");
    memcpy(resp + n, body, body_len); /* raw bytes, NULs preserved */
    n += (int)body_len;
    n += snprintf(resp + n, cap - (size_t)n, "\n");
  }

  char hdr[160];
  int hl = snprintf(hdr, sizeof(hdr),
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n"
                    "Content-Length: %d\r\nConnection: close\r\n\r\n",
                    n > 0 ? n : 0);
  if (write_all(cfd, hdr, (size_t)hl) == 0 && n > 0) write_all(cfd, resp, (size_t)n);
  free(resp);
  free(body);
}

int main(int argc, char **argv) {
  signal(SIGPIPE, SIG_IGN); /* broken pipe -> just drop the connection */
  const char *ps = getenv("PORT");
  if (!ps || !*ps) ps = argc > 1 ? argv[1] : "8080";
  long port = strtol(ps, NULL, 10);
  if (port <= 0 || port > 65535) port = 8080;

  int lfd = socket(AF_INET, SOCK_STREAM, 0);
  if (lfd < 0) { perror("socket"); return 1; }
  int one = 1;
  setsockopt(lfd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
  struct timeval tv = {5, 0}; /* per-connection: don't wedge on idle clients */
  struct sockaddr_in addr;
  memset(&addr, 0, sizeof(addr));
  addr.sin_family = AF_INET;
  addr.sin_port = htons((unsigned short)port);
  addr.sin_addr.s_addr = htonl(INADDR_ANY);
  if (bind(lfd, (struct sockaddr *)&addr, sizeof(addr)) < 0) { perror("bind"); return 1; }
  if (listen(lfd, 64) < 0) { perror("listen"); return 1; }
  fprintf(stderr, "echo: listening on 0.0.0.0:%ld\n", port);
  for (;;) { /* sequential accept loop; per-connection errors never kill us */
    struct sockaddr_in peer;
    socklen_t plen = sizeof(peer);
    int cfd = accept(lfd, (struct sockaddr *)&peer, &plen);
    if (cfd < 0) {
      if (errno != EINTR) perror("accept");
      continue;
    }
    /* one stderr line per request -> container log: lets e2e observe the
     * router's round-robin distribution across replicas (S6) */
    fprintf(stderr, "echo: connection from %s:%d\n", inet_ntoa(peer.sin_addr), ntohs(peer.sin_port));
    setsockopt(cfd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    handle(cfd);
    close(cfd); /* no keep-alive: respond then close */
  }
}
EOF
(cd "$WORK" && "$CC" -static -Os -o echo echo.c)
# Tolerant sanity check: file/readelf may or may not exist; only hard-require
# an executable, non-empty binary.
[[ -x "$WORK/echo" && -s "$WORK/echo" ]] || { echo "error: echo binary not built" >&2; exit 1; }
if command -v file >/dev/null && file "$WORK/echo" | grep -qi 'statically linked'; then
  echo "echo: static ELF confirmed by file(1)"
fi

# --- (b) deterministic layer tar (/bin/echo, mode 0755, owner 0:0, mtime @0) -
ROOTDIR="$WORK/rootdir"
mkdir -p "$ROOTDIR/bin"
install -m 0755 "$WORK/echo" "$ROOTDIR/bin/echo"
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
{"architecture":"amd64","os":"linux","created":"1970-01-01T00:00:00Z","config":{"Entrypoint":["/bin/echo"],"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],"User":"0:0","Labels":{"io.init-pro.built-by":"build-echo-image.sh"}},"rootfs":{"type":"layers","diff_ids":["sha256:$DIFF_ID"]},"history":[{"created":"1970-01-01T00:00:00Z","created_by":"init-pro airgap echo"}]}
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
