#!/usr/bin/env bash
# T4.11 / Q48: end-to-end exercise of the built-in engine control protocol
# (`init-pro engine serve --socket`). This is the P1 gate: a real root host,
# the single-threaded engine process, and a protocol client speaking the
# internal frame format (`u32 LE len | u8 kind | payload`).
#
# Cases:
#   B1 ping/pong                  B5 raw non-UTF8 stdout stays raw
#   B2 run: stream/exit events    B6 malformed request -> error + close
#   B3 exec failure: 127 surface  B7 shutdown -> bye + process exit 0
#   B4 cgroup limits over the wire
#
# Preconditions: PRE-BUILT binary, real root, static busybox, python3 for the
# wire client. Missing inputs => "SKIP builtin-runtime-e2e:" + exit 0 (Q25);
# a protocol violation is a hard FAIL.
#
# Usage: scripts/builtin-runtime-e2e.sh
#   INIT_PRO_BIN=...      reuse a specific binary (default target/debug/init-pro)
#   INIT_PRO_BUSYBOX=...  override the static busybox (default /usr/bin/busybox)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"
BUSYBOX="${INIT_PRO_BUSYBOX:-/usr/bin/busybox}"

skip() { echo "SKIP builtin-runtime-e2e: $*"; exit 0; }

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
[[ "$(id -u)" -eq 0 ]] || skip "requires real root (no user namespaces)"
[[ -x "$BUSYBOX" ]] || skip "no static busybox at $BUSYBOX"
command -v python3 >/dev/null || skip "python3 is required for the wire client"
if ldd "$BUSYBOX" >/dev/null 2>&1; then
  skip "$BUSYBOX is dynamically linked; the empty rootfs has no libs"
fi

TMP="$(mktemp -d /tmp/init-pro-engine-e2e.XXXXXX)"
ROOTFS="$TMP/rootfs"
SOCK="$TMP/engine.sock"
LOG="$TMP/engine.log"
ENGINE_PID=""
cleanup() {
  if [[ -n "$ENGINE_PID" ]] && kill -0 "$ENGINE_PID" 2>/dev/null; then
    kill "$ENGINE_PID" 2>/dev/null || true
    wait "$ENGINE_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

mkdir -p "$ROOTFS"/{bin,proc,sys,dev,tmp,etc}
chmod 755 "$ROOTFS" "$ROOTFS"/bin "$ROOTFS"/etc "$ROOTFS"/tmp
: >"$ROOTFS/bin/busybox"
chmod 644 "$ROOTFS/bin/busybox"

"$BIN" engine serve --socket "$SOCK" 2>"$LOG" &
ENGINE_PID=$!

for _ in $(seq 100); do
  [[ -S "$SOCK" ]] && break
  kill -0 "$ENGINE_PID" 2>/dev/null || break
  sleep 0.05
done

if [[ ! -S "$SOCK" ]]; then
  set +e
  wait "$ENGINE_PID"
  ERC=$?
  set -e
  ENGINE_PID=""
  if grep -q "preflight failed" "$LOG"; then
    skip "$(head -1 "$LOG")"
  fi
  echo "FAIL engine serve did not create $SOCK (exit $ERC)" >&2
  sed 's/^/      /' "$LOG" >&2
  exit 1
fi

if ! python3 - "$SOCK" "$ROOTFS" "$BUSYBOX" <<'PY'
import json
import os
import socket
import struct
import sys

sock_path, rootfs, busybox = sys.argv[1], sys.argv[2], sys.argv[3]
MOUNT = [{"source": busybox, "target": "/bin/busybox", "readonly": False}]


def connect():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(20)
    s.connect(sock_path)
    return s


def recv_exact(s, n):
    chunks = []
    while n:
        chunk = s.recv(n)
        if not chunk:
            raise EOFError("engine closed the connection")
        chunks.append(chunk)
        n -= len(chunk)
    return b"".join(chunks)


def send(s, kind, payload):
    s.sendall(struct.pack("<I", len(payload)) + bytes([kind]) + payload)


def send_json(s, obj):
    send(s, 1, json.dumps(obj, separators=(",", ":")).encode())


def recv_frame(s):
    header = recv_exact(s, 5)
    length = struct.unpack("<I", header[:4])[0]
    return header[4], recv_exact(s, length)


def recv_event(s):
    kind, payload = recv_frame(s)
    assert kind == 1, f"expected a JSON frame, got kind {kind}: {payload!r}"
    return json.loads(payload)


def run(s, spec):
    send_json(s, {"op": "run", "spec": spec})
    events, out, err = [], b"", b""
    while True:
        kind, payload = recv_frame(s)
        if kind == 2:
            out += payload
        elif kind == 3:
            err += payload
        elif kind == 1:
            event = json.loads(payload)
            events.append(event)
            if event.get("event") in ("exited", "error"):
                return events, out, err
        else:
            raise AssertionError(f"unexpected frame kind {kind}")
    return events, out, err


s = connect()

# B1: liveness.
send_json(s, {"op": "ping"})
assert recv_event(s) == {"event": "pong"}, "B1 ping/pong mismatch"
print("OK   B1 ping/pong")

# B2: started -> raw stdout/stderr -> exited, in order.
events, out, err = run(
    s,
    {
        "id": "b2",
        "rootfs": rootfs,
        "argv": ["/bin/busybox", "sh", "-c", "echo out; echo err >&2; exit 7"],
        "mounts": MOUNT,
    },
)
assert events[0].get("event") == "started" and events[0].get("id") == "b2", events
assert events[0].get("pid", 0) > 0, events
assert events[-1] == {"event": "exited", "id": "b2", "code": 7}, events
assert out == b"out\n", f"B2 stdout {out!r}"
assert err == b"err\n", f"B2 stderr {err!r}"
print("OK   B2 run streams + exit code 7")

# B3: a missing executable reports Started -> stderr -> Exited(127).
events, out, err = run(
    s,
    {
        "id": "b3",
        "rootfs": rootfs,
        "argv": ["/bin/nope"],
        "mounts": MOUNT,
    },
)
assert events[0].get("event") == "started", events
assert events[-1].get("event") == "exited" and events[-1].get("code") == 127, events
assert b"No such file" in err, f"B3 stderr {err!r}"
print("OK   B3 exec failure surfaces as exit 127")

# B4: cgroup limits travel through ContainerSpec and are cleaned up.
cg = f"e2e-{os.getpid()}"
events, out, err = run(
    s,
    {
        "id": "b4",
        "rootfs": rootfs,
        "argv": ["/bin/busybox", "cat", "/sys/fs/cgroup/pids.max"],
        "mounts": MOUNT,
        "cgroup_name": cg,
        "pids_max": 16,
    },
)
assert events[-1].get("event") == "exited" and events[-1].get("code") == 0, events
assert out == b"16\n", f"B4 pids.max {out!r}"
assert not os.path.isdir(f"/sys/fs/cgroup/init-pro/{cg}"), "B4 cgroup leaked"
print("OK   B4 cgroup limits over the wire (pids.max=16, cleaned)")

# B5: stdout is a raw byte channel, not a UTF-8/base64 protocol.
events, out, err = run(
    s,
    {
        "id": "b5",
        "rootfs": rootfs,
        "argv": ["/bin/busybox", "sh", "-c", "printf 'a\\377b\\n'"],
        "mounts": MOUNT,
    },
)
assert events[-1].get("event") == "exited" and events[-1].get("code") == 0, events
assert out == b"a\xffb\n", f"B5 raw stdout {out!r}"
print("OK   B5 non-UTF8 stdout preserved")

# B6: malformed control JSON -> error event, then the session closes.
send(s, 1, b"{not json")
event = recv_event(s)
assert event.get("event") == "error" and event.get("message"), event
assert s.recv(1) == b"", "B6 session did not close after the error"
s.close()
print("OK   B6 malformed request -> error + close")

# B7: shutdown is acknowledged, then the engine process exits.
s = connect()
send_json(s, {"op": "shutdown"})
assert recv_event(s) == {"event": "bye"}, "B7 shutdown not acknowledged"
s.close()
print("OK   B7 shutdown -> bye")
PY
then
  echo "FAIL builtin-runtime-e2e: protocol client failed" >&2
  exit 1
fi

for _ in $(seq 100); do
  kill -0 "$ENGINE_PID" 2>/dev/null || break
  sleep 0.05
done
if kill -0 "$ENGINE_PID" 2>/dev/null; then
  echo "FAIL engine did not exit after shutdown" >&2
  exit 1
fi
set +e
wait "$ENGINE_PID"
ERC=$?
set -e
ENGINE_PID=""
if [[ "$ERC" -ne 0 ]]; then
  echo "FAIL engine serve exited $ERC after shutdown" >&2
  sed 's/^/      /' "$LOG" >&2
  exit 1
fi
grep -q "listening on" "$LOG" || { echo "FAIL engine never logged its listener" >&2; exit 1; }

echo
echo "builtin-runtime-e2e: B1-B7 passed (engine exited 0)"
