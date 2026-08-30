#!/usr/bin/env bash
# T-anyhome (Q36): real-machine e2e probe over the embedded anyhome NFSv3
# memory plane. Boots `init-pro server` with the NFS plane on, then asserts:
#
#   A1  root-mount roundtrip (REQUIRED): a keeper-simulated workflow dir
#       pre-created under <data>/anyhome/fs/workflow/probe-wf is reachable
#       through `mount.nfs 127.0.0.1:/` (vers=3,tcp,nolock): a file written
#       THROUGH the mount reads back through it and on the server's disk.
#   P2  subpath-mount probe (REQUIRED): `mount.nfs
#       127.0.0.1:/workflow/probe-wf` directly. Whether nfsserve accepts a
#       subpath of the `/` export is UNKNOWN upstream (coordination item
#       A2) -- this probe pins the observed answer: a rejection here FAILS
#       the run (the mount being rejected stays an upstream item).
#   K1  keeper dir creation via the apiserver (INFORMATIONAL): POST a
#       minimal argoproj.io Workflow, then poll <=15s for the keeper-made
#       `<data>/anyhome/fs/workflow/<name>` directory. Controllers are
#       DISABLED for this run and that is fine: the keeper reads plain
#       stored objects (no reconcile needed), so the probe doubles as proof
#       that the store->keeper seam works without the controller manager.
#
# SKIP doctrine: a missing mount capability is a configuration state, not a
# conformance break -- no mount.nfs, or no root, prints SKIP and exits 0.
# Missing curl/python3 only degrade the K1 probe (and the port picker).
#
# Usage: scripts/anyhome-e2e.sh
#   INIT_PRO_BIN=...  reuse a specific binary (default target/debug/init-pro)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }

skip() { echo "SKIP anyhome-e2e: $*"; exit 0; }

# --- preflight (SKIP-grade inputs) -------------------------------------------
MOUNT_NFS="$(command -v mount.nfs 2>/dev/null || true)"
for c in /sbin/mount.nfs /usr/sbin/mount.nfs /usr/local/sbin/mount.nfs; do
  [[ -n "$MOUNT_NFS" ]] && break
  [[ -x "$c" ]] && MOUNT_NFS="$c"
done
[[ -n "$MOUNT_NFS" ]] || skip "no mount.nfs on PATH or /sbin (NFS client tools absent)"
[[ "$(id -u)" == "0" ]] || skip "needs root (NFS mount + portmap require CAP_SYS_ADMIN)"

HAVE_CURL=1; command -v curl >/dev/null 2>&1 || HAVE_CURL=0
HAVE_PY=1;   command -v python3 >/dev/null 2>&1 || HAVE_PY=0

pick_port() {
  if [[ "$HAVE_PY" == "1" ]]; then
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null \
      || echo $((20000 + RANDOM % 10000))
  else
    # Degraded picker: python3 missing only downgrades K1, not the run.
    echo $((20000 + RANDOM % 10000))
  fi
}

# stop_pid: TERM first, bounded wait, then force (KILL).
stop_pid() {
  local p="$1" i
  [[ -n "$p" ]] || return 0
  kill -TERM "$p" 2>/dev/null || true
  for i in $(seq 1 50); do
    kill -0 "$p" 2>/dev/null || return 0
    sleep 0.1
  done
  kill -KILL "$p" 2>/dev/null || true
  wait "$p" 2>/dev/null || true
}

umount_quiet() {
  local m="$1"
  umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
}

PASS=0; FAIL=0; WARN=0
ok()   { echo "OK    $*"; PASS=$((PASS+1)); }
bad()  { echo "FAIL  $*"; FAIL=$((FAIL+1)); }
warn() { echo "WARN  $*"; WARN=$((WARN+1)); }

PORT="$(pick_port)"
API_PORT="$(pick_port)"
BASE="http://127.0.0.1:$API_PORT"   # apiserver port (isolated per run)
TMP="$(mktemp -d /tmp/anyhome-e2e.XXXXXX)"
DATA="$TMP/data"
SRV_LOG="$TMP/server.log"
SRV_PID=""
MNT1="$TMP/mnt-root"
MNT2="$TMP/mnt-sub"

cleanup() {
  trap - EXIT INT TERM
  echo
  echo "anyhome e2e: tearing down ..."
  umount_quiet "$MNT1"
  umount_quiet "$MNT2"
  stop_pid "$SRV_PID"
  rm -rf "$TMP" 2>/dev/null || true
  echo "anyhome e2e: bye."
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# --- boot the server with the anyhome plane on -------------------------------
# Flags verified against crates/cli/src/cmd/server.rs (T-anyhome/Q36 wired):
# controllers/scheduler/kube-proxy off to keep the probe single-purpose.
"$BIN" server \
  --anyhome-addr "127.0.0.1:$PORT" \
  --disable-controller-manager \
  --disable-scheduler \
  --disable-kube-proxy \
  --bind-address 127.0.0.1 \
  --https-listen-port "$API_PORT" \
  --data-dir "$DATA" >"$SRV_LOG" 2>&1 &
SRV_PID=$!

READY=0
for _ in $(seq 1 60); do
  # anyhome multiplexes NFS/MOUNT/portmap on the ONE port: a TCP connect is
  # the readiness probe (no HTTP on that port).
  if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
    exec 3>&- 3<&- || true
    READY=1
    break
  fi
  if ! kill -0 "$SRV_PID" 2>/dev/null; then
    break
  fi
  sleep 0.5
done
if [[ "$READY" != "1" ]]; then
  echo "--- server log ($SRV_LOG) ---"; cat "$SRV_LOG" || true
  bad "server never opened the anyhome port 127.0.0.1:$PORT"
  echo "anyhome e2e: $PASS passed, $FAIL failed, $WARN warnings"
  exit 1
fi
ok "server up, anyhome listening on 127.0.0.1:$PORT"

MOUNT_OPTS="vers=3,tcp,nolock,noacl,port=$PORT,mountport=$PORT"

# --- A1: root mount roundtrip (REQUIRED) -------------------------------------
mkdir -p "$DATA/anyhome/fs/workflow/probe-wf"   # simulates the keeper
mkdir -p "$MNT1"
A1=""
if "$MOUNT_NFS" -o "$MOUNT_OPTS" "127.0.0.1:/" "$MNT1" 2>"$TMP/a1.err"; then
  echo "probe anyhome-e2e A1" > "$MNT1/workflow/probe-wf/probe.txt" \
    && grep -q "probe anyhome-e2e A1" "$MNT1/workflow/probe-wf/probe.txt" \
    && grep -q "probe anyhome-e2e A1" "$DATA/anyhome/fs/workflow/probe-wf/probe.txt" \
    && A1=1
  umount_quiet "$MNT1"
  if [[ -n "$A1" ]]; then
    ok "A1  root mount roundtrip: write+read via 127.0.0.1:/ under /workflow/probe-wf"
  else
    bad "A1  root mount worked but the probe file roundtrip failed"
  fi
else
  bad "A1  root mount failed: $(cat "$TMP/a1.err" 2>/dev/null || echo '?')"
fi

# --- P2: subpath mount probe (REQUIRED, A2) -----------------------------------
mkdir -p "$MNT2"
P2=""
if "$MOUNT_NFS" -o "$MOUNT_OPTS" "127.0.0.1:/workflow/probe-wf" "$MNT2" 2>"$TMP/p2.err"; then
  echo "probe anyhome-e2e P2" > "$MNT2/p2.txt" \
    && grep -q "probe anyhome-e2e P2" "$MNT2/p2.txt" \
    && P2=1
  umount_quiet "$MNT2"
  if [[ -n "$P2" ]]; then
    ok "P2  subpath mount ACCEPTED (host:/workflow/probe-wf mounts + roundtrips)"
  else
    bad "P2  subpath mount worked but the probe file roundtrip failed"
  fi
else
  bad "P2: subpath mount rejected ($(head -c 200 "$TMP/p2.err" 2>/dev/null || echo 'MNT3ERR?')) -- anyhome upstream coordination item (A2)"
fi

# --- K1: keeper dir creation via the apiserver (informational) ----------------
# curl/python3 degraded at preflight; apiserver must be up for this probe.
if [[ "$HAVE_CURL" == "1" ]]; then
  cat > "$TMP/k1.json" <<'JSON'
{"apiVersion":"argoproj.io/v1alpha1","kind":"Workflow","metadata":{"name":"anyhome-k1-wf","namespace":"default"},"spec":{}}
JSON
  CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary @"$TMP/k1.json" "$BASE/apis/argoproj.io/v1alpha1/namespaces/default/workflows" || echo 000)"
  K1=""
  if [[ "$CODE" == "201" || "$CODE" == "200" || "$CODE" == "409" ]]; then
    for _ in $(seq 1 30); do   # <=15s: keeper beat is 5s, first sync immediate
      [[ -d "$DATA/anyhome/fs/workflow/anyhome-k1-wf" ]] && { K1=1; break; }
      sleep 0.5
    done
  fi
  if [[ -n "$K1" ]]; then
    ok "K1  keeper created <data>/anyhome/fs/workflow/anyhome-k1-wf after the Workflow POST"
  else
    warn "K1: no keeper dir within 15s (POST code: $CODE) -- informational, see $SRV_LOG"
  fi
else
  warn "K1: curl absent -- probe degraded at preflight (SKIP-grade, not a failure)"
fi

# --- summary: exit 0 only if the REQUIRED probes (A1+P2) passed; K1 stays WARN-able
echo
echo "anyhome e2e: $PASS passed, $FAIL failed, $WARN warnings"
REQ_FAILED=""
[[ -n "${A1:-}" ]] || REQ_FAILED="A1"
[[ -n "${P2:-}" ]] || REQ_FAILED="${REQ_FAILED:+$REQ_FAILED+}P2"
if [[ -z "$REQ_FAILED" ]]; then
  echo "ALL anyhome e2e required checks passed (A1+P2)"
  exit 0
fi
echo "anyhome e2e FAILED (required probe(s) $REQ_FAILED did not pass)" >&2
exit 1
