#!/usr/bin/env bash
# T4.11 / Q48: end-to-end exercise of the built-in engine control protocol
# (`init-pro engine serve --socket`). This is the P1 gate: a real root host,
# the single-threaded engine process, and a protocol client speaking the
# internal frame format (`u32 LE len | u8 kind | payload`).
#
# Cases:
#   B1 ping/pong                  B8 signal: SIGKILL -> 137, TERM trap -> 23
#   B2 run: stream/exit events    B9 CRI log lines (P/F per stream)
#   B3 exec failure: 127 surface  B10 pause engine: pong pid + netns join
#   B4 cgroup limits over the wire  B11 live-socket refusal (Q49)
#   B5 raw non-UTF8 stdout        B12 malformed request -> error, survives
#   B6 exec: PID 1 visible in ns  B13 shutdown -> bye + process exit 0
#   B7 exec: merged streams + code  B14 host_root wire run (Q60: host root,
#                                    host netns, fresh /proc, private mounts)
#   N1 host-net pause == host netns (Q52)   N2 worker join stays host netns
#   N3-N5 builtin cluster (Q52): shared-netns pod serves a host port,
#   podIP==hostIP==node IP via REST, port released on delete.
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
NDD=""; NCLUSTER_SRV_PID=""; NCLUSTER_AGENT_PID=""
cleanup() {
  if [[ -n "$ENGINE_PID" ]] && kill -0 "$ENGINE_PID" 2>/dev/null; then
    kill "$ENGINE_PID" 2>/dev/null || true
    wait "$ENGINE_PID" 2>/dev/null || true
  fi
  for p in "$NCLUSTER_AGENT_PID" "$NCLUSTER_SRV_PID"; do
    if [[ -n "$p" ]] && kill -0 "$p" 2>/dev/null; then
      kill "$p" 2>/dev/null || true
      for _ in $(seq 1 40); do kill -0 "$p" 2>/dev/null || break; sleep 0.25; done
      kill -KILL "$p" 2>/dev/null || true
      wait "$p" 2>/dev/null || true
    fi
  done
  rm -rf "$NDD"
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

if ! python3 - "$SOCK" "$ROOTFS" "$BUSYBOX" "$BIN" <<'PY'
import json
import os
import re
import socket
import struct
import subprocess
import sys
import time

sock_path, rootfs, busybox, bin_path = (
    sys.argv[1],
    sys.argv[2],
    sys.argv[3],
    sys.argv[4],
)
MOUNT = [{"source": busybox, "target": "/bin/busybox", "readonly": False}]


def connect(path=None):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(20)
    s.connect(path or sock_path)
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

# B6: exec inside a running container sees the container's PID namespace
# (the `sleep 30` is PID 1 there). One in-flight exec per engine (Q50).
send_json(
    s,
    {
        "op": "run",
        "spec": {
            "id": "b6",
            "rootfs": rootfs,
            "argv": ["/bin/busybox", "sleep", "30"],
            "mounts": MOUNT,
        },
    },
)
started = recv_event(s)
assert started.get("event") == "started" and started.get("pid", 0) > 0, started


def exec_req(argv):
    send_json(s, {"op": "exec", "id": "b6", "argv": argv})
    events, out, err = [], b"", b""
    while True:
        kind, payload = recv_frame(s)
        if kind == 2:
            out += payload
        elif kind == 3:
            err += payload
        else:
            event = json.loads(payload)
            events.append(event)
            if event.get("event") in ("exec_exited", "error"):
                return events, out, err


events, out, err = exec_req(["/bin/busybox", "ps"])
assert events[0].get("event") == "exec_started" and events[0].get("exec_id") == "exec-1", events
assert events[0].get("pid", 0) > 0, events
assert events[-1] == {"event": "exec_exited", "exec_id": "exec-1", "code": 0}, events
assert b"sleep" in out, f"B6 ps output {out!r}"
print("OK   B6 exec sees PID 1 (sleep) in the container ns")

# B7: exec output merges raw kinds 2/3 and the exit code rides ExecExited.
events, out, err = exec_req(
    ["/bin/busybox", "sh", "-c", "echo xout; echo xerr >&2; exit 5"]
)
assert events[-1].get("code") == 5, events
assert out == b"xout\n" and err == b"xerr\n", (out, err)
print("OK   B7 exec merged streams + exit code 5")

# B8: Signal semantics (pid-ns rule: an ancestor's SIGTERM reaches the
# container init only if it trapped it; SIGKILL always lands). Retire
# b6's sleep with SIGKILL -> Exited 137, then the graceful path runs a
# trapping init's handler, then the engine still serves another run.
send_json(s, {"op": "signal", "id": "b6", "signo": 9})
assert recv_event(s) == {"event": "exited", "id": "b6", "code": 137}, "B8 SIGKILL must map to 128+9"
send_json(
    s,
    {
        "op": "run",
        "spec": {
            "id": "b8a",
            "rootfs": rootfs,
            "argv": [
                "/bin/busybox",
                "sh",
                "-c",
                "trap 'exit 23' TERM; while :; do sleep 1; done",
            ],
            "mounts": MOUNT,
        },
    },
)
started = recv_event(s)
assert started.get("event") == "started" and started.get("id") == "b8a", started
send_json(s, {"op": "signal", "id": "b8a", "signo": 15})
assert recv_event(s) == {"event": "exited", "id": "b8a", "code": 23}, "B8 graceful TERM must run the init's trap"
events, out, err = run(
    s,
    {
        "id": "b8b",
        "rootfs": rootfs,
        "argv": ["/bin/busybox", "echo", "again"],
        "mounts": MOUNT,
    },
)
assert events[-1].get("code") == 0 and out == b"again\n", (events, out)
print("OK   B8 signal: SIGKILL -> 137, TERM trap -> 23; engine serves the next run")

# B9: spec.log_path appends CRI lines "<RFC3339Nano> <stream> <P|F> <body>".
# `printf p` (no newline) flushes as P on the stream switch to stderr.
logfile = os.path.join(os.path.dirname(sock_path), "b9.cri.log")
events, out, err = run(
    s,
    {
        "id": "b9",
        "rootfs": rootfs,
        "argv": ["/bin/busybox", "sh", "-c", "printf p; echo e >&2"],
        "mounts": MOUNT,
        "log_path": logfile,
    },
)
assert events[-1].get("code") == 0, events
lines = open(logfile, encoding="utf-8").read().splitlines()
assert len(lines) == 2, lines
stamp = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{9}Z"
assert re.fullmatch(stamp + r" stdout P p", lines[0]), lines
assert re.fullmatch(stamp + r" stderr F e", lines[1]), lines
print("OK   B9 CRI log lines (stream-switch flush P, newline flush F)")

# B10: a --pause engine (Q49): pong carries the pause pid, a member
# engine-run --netns joins that netns, Run is refused, Shutdown exits 0.
tmp = os.path.dirname(sock_path)
pause_sock = os.path.join(tmp, "pause.sock")
with open(os.path.join(tmp, "pause.log"), "wb") as pause_log:
    pause = subprocess.Popen(
        [bin_path, "engine", "serve", "--socket", pause_sock, "--pause"],
        stderr=pause_log,
    )
for _ in range(200):
    if os.path.exists(pause_sock):
        break
    time.sleep(0.05)
assert os.path.exists(pause_sock), "B10 pause engine never created its socket"
psock = connect(pause_sock)
send_json(psock, {"op": "ping"})
pong = recv_event(psock)
assert pong.get("event") == "pong" and pong.get("pid", 0) > 0, pong
pause_pid = pong["pid"]
assert os.path.isdir(f"/proc/{pause_pid}"), f"B10 pause pid {pause_pid} not alive"
holder_netns = os.readlink(f"/proc/{pause_pid}/ns/net")
joined = subprocess.run(
    [
        bin_path,
        "engine-run",
        "--rootfs",
        rootfs,
        "--netns",
        f"/proc/{pause_pid}/ns/net",
        "--mount",
        f"{busybox}:/bin/busybox",
        "--",
        "/bin/busybox",
        "readlink",
        "/proc/self/ns/net",
    ],
    capture_output=True,
    timeout=60,
)
assert joined.returncode == 0, joined.stderr
assert joined.stdout.decode().strip() == holder_netns, (
    f"B10 netns join {joined.stdout!r} != {holder_netns}"
)
send_json(
    psock,
    {
        "op": "run",
        "spec": {"id": "x", "rootfs": rootfs, "argv": ["/bin/busybox", "true"], "mounts": MOUNT},
    },
)
refused = recv_event(psock)
assert refused.get("event") == "error" and "cannot run" in refused.get("message", ""), refused
send_json(psock, {"op": "shutdown"})
assert recv_event(psock) == {"event": "bye"}
psock.close()
assert pause.wait(timeout=15) == 0, "B10 pause engine did not exit 0"
print("OK   B10 pause engine: pong pid, netns join, run refused, exit 0")

# N1 (Q52): --host-net keeps the pause sandbox in the host netns — no
# CLONE_NEWNET, no lo bring-up. pong pid alive + sandbox netns == the
# test process's own netns.
host_netns = os.readlink("/proc/self/ns/net")
hn_sock = os.path.join(tmp, "hn-pause.sock")
with open(os.path.join(tmp, "hn-pause.log"), "wb") as hn_log:
    hn = subprocess.Popen(
        [bin_path, "engine", "serve", "--socket", hn_sock, "--pause", "--host-net"],
        stderr=hn_log,
    )
for _ in range(200):
    if os.path.exists(hn_sock):
        break
    time.sleep(0.05)
assert os.path.exists(hn_sock), "N1 host-net pause engine never bound its socket"
hsock = connect(hn_sock)
send_json(hsock, {"op": "ping"})
pong = recv_event(hsock)
assert pong.get("event") == "pong" and pong.get("pid", 0) > 0, pong
hn_pid = pong["pid"]
assert os.path.isdir(f"/proc/{hn_pid}"), f"N1 pause pid {hn_pid} not alive"
assert os.readlink(f"/proc/{hn_pid}/ns/net") == host_netns, (
    f"N1 pause netns must equal host netns (Q52), got pid {hn_pid}"
)
# N2 (Q52): a worker joining /proc/<host-net-pause>/ns/net lands in the
# host netns too — same assertion shape as B10, opposite netns source.
joined_hn = subprocess.run(
    [
        bin_path,
        "engine-run",
        "--rootfs",
        rootfs,
        "--netns",
        f"/proc/{hn_pid}/ns/net",
        "--mount",
        f"{busybox}:/bin/busybox",
        "--",
        "/bin/busybox",
        "readlink",
        "/proc/self/ns/net",
    ],
    capture_output=True,
    timeout=60,
)
assert joined_hn.returncode == 0, joined_hn.stderr
assert joined_hn.stdout.decode().strip() == host_netns, (
    f"N2 worker netns {joined_hn.stdout!r} != host {host_netns}"
)
send_json(hsock, {"op": "shutdown"})
assert recv_event(hsock) == {"event": "bye"}
hsock.close()
assert hn.wait(timeout=15) == 0, "N1 host-net pause engine did not exit 0"
print("OK   N1 pause --host-net: pong pid alive, sandbox netns == host netns (Q52)")
print("OK   N2 worker joins host-net pause: readlink == host netns (Q52)")

# B14 (T4.15, Q60): wire-level host_root — rootfs "/" + host_root: true
# over the frame protocol keeps the HOST root (host marker visible, host
# /dev populated) and the HOST network namespace (no NEWNET, no setns),
# with a FRESH /proc for the new PID namespace and private mount
# propagation (no shared: tags in mountinfo).
b14_marker = os.path.join("/tmp", f"b14-q60-{os.getpid()}")
with open(b14_marker, "w") as f:
    f.write("hd-wire-secret")
try:
    events, out, err = run(
        s,
        {
            "id": "b14",
            "rootfs": "/",
            "host_root": True,
            "argv": [
                busybox,
                "sh",
                "-c",
                "echo M=$(cat " + b14_marker + "); "
                "echo D=$(test -c /dev/null && echo char); "
                "echo P=$(readlink /proc/self/ns/pid); "
                "echo N=$(readlink /proc/self/ns/net); "
                "echo S=$(grep -c 'shared:' /proc/self/mountinfo)",
            ],
        },
    )
finally:
    os.unlink(b14_marker)
assert events[-1] == {"event": "exited", "id": "b14", "code": 0}, events
wire = dict(
    line.split("=", 1) for line in out.decode().splitlines() if "=" in line
)
assert wire.get("M") == "hd-wire-secret", f"B14 host marker missing: {wire!r}"
assert wire.get("D") == "char", f"B14 host /dev not visible: {wire!r}"
assert wire.get("P") and wire["P"] != os.readlink(
    "/proc/self/ns/pid"
), f"B14 fresh PID ns expected: {wire!r}"
assert wire.get("N") == host_netns, f"B14 must stay in the host netns: {wire!r}"
assert wire.get("S") == "0", f"B14 mount tree must be private: {wire!r}"
print("OK   B14 host_root over the wire: host root + /dev, host netns, fresh /proc, private mounts")

# B11: a second engine must refuse a live engine's socket (Q49) and the
# live engine keeps answering.
live_sock = os.path.join(tmp, "live.sock")
with open(os.path.join(tmp, "live.log"), "wb") as live_log:
    live = subprocess.Popen(
        [bin_path, "engine", "serve", "--socket", live_sock], stderr=live_log
    )
for _ in range(200):
    if os.path.exists(live_sock):
        break
    time.sleep(0.05)
assert os.path.exists(live_sock), "B11 live engine never created its socket"
with open(os.path.join(tmp, "dupe.log"), "wb") as dupe_log:
    dupe = subprocess.Popen(
        [bin_path, "engine", "serve", "--socket", live_sock], stderr=dupe_log
    )
    assert dupe.wait(timeout=15) == 1, "B11 the second engine must refuse (exit 1)"
dupe_out = open(os.path.join(tmp, "dupe.log"), "rb").read()
assert b"refusing to replace" in dupe_out, dupe_out
lsock = connect(live_sock)
send_json(lsock, {"op": "ping"})
assert recv_event(lsock) == {"event": "pong"}, "B11 the live engine must keep answering"
send_json(lsock, {"op": "shutdown"})
assert recv_event(lsock) == {"event": "bye"}
lsock.close()
assert live.wait(timeout=15) == 0
print("OK   B11 a live engine's socket is never replaced (Q49)")

# B12: malformed control JSON -> error event; since M2 the session
# survives it (send_error + continue), so a follow-up ping still works.
send(s, 1, b"{not json")
event = recv_event(s)
assert event.get("event") == "error" and event.get("message"), event
send_json(s, {"op": "ping"})
assert recv_event(s) == {"event": "pong"}, "B12 session must survive a bad request"
s.close()
print("OK   B12 malformed request -> error; session survives")

# B13: shutdown is acknowledged, then the engine process exits.
s = connect()
send_json(s, {"op": "shutdown"})
assert recv_event(s) == {"event": "bye"}, "B13 shutdown not acknowledged"
s.close()
print("OK   B13 shutdown -> bye")
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

# --- N3-N5 (Q52): builtin cluster — pods share the host netns -------------
# Boots server + agent (--container-engine builtin, NO containerd bundle,
# NO CNI): a pod whose container binds a host port is reachable from the
# host, reports podIP == hostIP == node IP, and releases the port on delete.
RUN_CLUSTER=1
if ! command -v curl >/dev/null 2>&1; then
  echo "SKIP N3-N5 (no curl for the builtin cluster cases)"
  RUN_CLUSTER=0
fi
if [[ "$RUN_CLUSTER" -eq 1 ]]; then
  pick_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
  }
  fetch() {
    python3 -c '
import sys, urllib.request
try:
    sys.stdout.write(urllib.request.urlopen(sys.argv[1], timeout=5).read().decode())
except Exception:
    sys.exit(1)
' "$1" 2>/dev/null || true
  }
  nok() { echo "OK   $*"; }
  nfail() { echo "FAIL $*" >&2; exit 1; }

  API_PORT="$(pick_port)"
  BASE="http://127.0.0.1:${API_PORT}"
  NODE="hn-node"
  WEB_PORT="$(pick_port)"
  NDD="$(mktemp -d /tmp/init-pro-hostnet-e2e.XXXXXX)"
  NCLUSTER_SRV_LOG="$NDD/server.log"; NCLUSTER_AGENT_LOG="$NDD/agent.log"

  # Pod rootfs: host-dir image (T4.12 stopgap) with busybox baked in; the
  # container is `busybox httpd -f -p PORT` serving / == the rootfs root.
  NRFS="$NDD/webrootfs"
  mkdir -p "$NRFS/bin"
  cp "$BUSYBOX" "$NRFS/bin/busybox"
  chmod 755 "$NRFS/bin/busybox"
  echo "hello-hostnet-q52" >"$NRFS/index.html"

  "$BIN" server --data-dir "$NDD/srv" --bind-address 127.0.0.1 \
    --https-listen-port "$API_PORT" >"$NCLUSTER_SRV_LOG" 2>&1 &
  NCLUSTER_SRV_PID=$!
  for _ in $(seq 1 60); do
    grep -q "discovery listening" "$NCLUSTER_SRV_LOG" && break; sleep 0.1
  done
  grep -q "discovery listening" "$NCLUSTER_SRV_LOG" \
    || nfail "N3 server never reported listening ($(tail -5 "$NCLUSTER_SRV_LOG" | tr '\n' ' '))"

  "$BIN" agent --data-dir "$NDD/agent" --token dev --server "$BASE" \
    --node-name "$NODE" --container-engine builtin >"$NCLUSTER_AGENT_LOG" 2>&1 &
  NCLUSTER_AGENT_PID=$!
  for _ in $(seq 1 300); do
    grep -q "kubelet started (T4.12 built-in engine)" "$NCLUSTER_AGENT_LOG" && break; sleep 0.1
  done
  grep -q "kubelet started (T4.12 built-in engine)" "$NCLUSTER_AGENT_LOG" \
    || nfail "N3 builtin agent never came up ($(tail -5 "$NCLUSTER_AGENT_LOG" | tr '\n' ' '))"
  # The skip line lands AFTER the agent-runner staging (T7.7 D7 binary
  # copy), tens of ms later — poll for it instead of racing one grep.
  for _ in $(seq 1 300); do
    grep -q "flannel skipped (Q52)" "$NCLUSTER_AGENT_LOG" && break; sleep 0.1
  done
  grep -q "flannel skipped (Q52)" "$NCLUSTER_AGENT_LOG" \
    || nfail "N3 builtin agent must skip flanneld (Q52)"

  CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data-binary @- "$BASE/api/v1/namespaces/default/pods" <<EOF
{"apiVersion":"v1","kind":"Pod","metadata":{"name":"hostnet-web","namespace":"default"},
 "spec":{"containers":[{"name":"web","image":"$NRFS",
   "command":["/bin/busybox","httpd","-f","-p","$WEB_PORT"]}]}}
EOF
)"
  [[ "$CODE" == "201" ]] || nfail "N3 pod POST -> $CODE"

  # N3: the pod's host port answers from the host (shared netns, Q52).
  BODY=""
  for _ in $(seq 1 120); do
    BODY="$(fetch "http://127.0.0.1:${WEB_PORT}/")"
    [[ "$BODY" == "hello-hostnet-q52" ]] && break
    sleep 0.5
  done
  [[ "$BODY" == "hello-hostnet-q52" ]] \
    || nfail "N3 host port :$WEB_PORT never served the pod (got ${BODY:-<refused>}; agent log: $NCLUSTER_AGENT_LOG)"
  nok "N3 builtin pod serves host port $WEB_PORT (host netns shared, Q52)"

  # N4: the served pod JSON reports podIP == hostIP == the node's
  # InternalIP. (The bundled kubectl `get` speaks only the workflow/agent
  # surfaces today — T3.1b/T7.3 scope — so this reads the REST pod object.)
  NODE_IP="$(curl -s "$BASE/api/v1/nodes/$NODE" | python3 -c '
import json,sys
n=json.load(sys.stdin)
for a in n.get("status",{}).get("addresses",[]):
    if a.get("type")=="InternalIP": print(a["address"]); break
')"
  [[ -n "$NODE_IP" ]] || nfail "N4 node $NODE has no InternalIP"
  CODE="$(curl -s -o "$NDD/pod.json" -w '%{http_code}' "$BASE/api/v1/namespaces/default/pods/hostnet-web")"
  [[ "$CODE" == "200" ]] || nfail "N4 pod GET -> $CODE"
  python3 - "$NODE_IP" "$NDD/pod.json" <<'PY2' || nfail "N4 podIP/hostIP mismatch (expected node IP $NODE_IP)"
import json,sys
node_ip, path = sys.argv[1], sys.argv[2]
s = json.load(open(path))["status"]
assert s.get("podIP") == node_ip, f"podIP {s.get('podIP')!r} != node {node_ip!r}"
assert s.get("hostIP") == node_ip, f"hostIP {s.get('hostIP')!r} != node {node_ip!r}"
PY2
  nok "N4 podIP == hostIP == node IP ($NODE_IP, Q52)"

  # N5: delete the pod -> the host port is released (connect refused).
  curl -s -o /dev/null -X DELETE "$BASE/api/v1/namespaces/default/pods/hostnet-web"
  RELEASED=0
  for _ in $(seq 1 120); do
    if [[ -z "$(fetch "http://127.0.0.1:${WEB_PORT}/")" ]]; then RELEASED=1; break; fi
    sleep 0.5
  done
  [[ "$RELEASED" -eq 1 ]] || nfail "N5 port $WEB_PORT still answering after pod delete"
  nok "N5 pod delete releases the host port (Q52)"
fi

echo
if [[ "$RUN_CLUSTER" -eq 1 ]]; then
  echo "builtin-runtime-e2e: B1-B13 + N1-N5 passed (engine exited 0; Q52 host-netns pods)"
else
  echo "builtin-runtime-e2e: B1-B13 + N1-N2 passed (engine exited 0)"
fi
