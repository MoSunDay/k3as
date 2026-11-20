#!/usr/bin/env bash
# T4.11 / Q48: `init-pro engine-run` smoke matrix on a REAL root host.
#
# The built-in engine is not run in CI containers: it needs real root (no
# user namespace), mount_setattr(2) (Linux 5.12+), and a static busybox to
# populate the deliberately empty rootfs. Everything else is self-contained.
#
# Cases:
#   E1 exit-code passthrough (42)        E5 PID 1 + cap-bounding drop
#   E2 missing executable -> 127         E6 read-only vs read-write bind mounts
#   E3 exec a directory  -> 126          E7 cgroup v2 limits visible + cleaned
#   E4 uid/gid/env/hostname              E8 --new-net gives a fresh netns
#   E9 --netns joins a live netns        E10 signal death: SIGKILL -> 137
#   E11 SIGTERM runs the trapping init   E12 --host-root: host root stays
#                                         (T4.15, Q60: fresh PID ns + /proc,
#                                          host netns + /dev, private mounts)
#
# SKIP semantics (Q25): no root, no static busybox, or a kernel without
# mount_setattr => one "SKIP engine-selftest:" line and exit 0. Any other
# failure is a real FAIL.
#
# Usage: scripts/engine-selftest.sh
#   INIT_PRO_BIN=...      reuse a specific binary (default target/debug/init-pro)
#   INIT_PRO_BUSYBOX=...  override the static busybox (default /usr/bin/busybox)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"
BUSYBOX="${INIT_PRO_BUSYBOX:-/usr/bin/busybox}"

skip() { echo "SKIP engine-selftest: $*"; exit 0; }

[[ -x "$BIN" ]] || { echo "error: $BIN not found. Run 'cargo build'." >&2; exit 1; }
[[ "$(id -u)" -eq 0 ]] || skip "requires real root (no user namespaces)"
[[ -x "$BUSYBOX" ]] || skip "no static busybox at $BUSYBOX"
if ldd "$BUSYBOX" >/dev/null 2>&1; then
  skip "$BUSYBOX is dynamically linked; the empty rootfs has no libs"
fi

TMP="$(mktemp -d /tmp/init-pro-engine-selftest.XXXXXX)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

PASS=0
FAIL=0
RC=0

ok()   { echo "OK   $*"; PASS=$((PASS + 1)); }
bad()  { echo "FAIL $*"; FAIL=$((FAIL + 1)); }

check_rc() { # check_rc <label> <want> <got>
  if [[ "$2" -eq "$3" ]]; then ok "$1 (exit $3)"; else bad "$1 (want exit $2, got $3)"; fi
}
check_has() { # check_has <label> <needle> <file>
  if grep -qF -- "$2" "$3"; then ok "$1"; else
    bad "$1 (missing [$2] in $(basename "$3"))"
    sed 's/^/      /' "$3" | head -5
  fi
}

# new_rootfs <tag>: an empty image with just the mount targets the test
# needs. `chmod 755` matters for --uid cases: the container user must be
# able to traverse `/` and `/bin`.
new_rootfs() {
  local d="$TMP/rootfs-$1"
  mkdir -p "$d"/{bin,proc,sys,dev,tmp,etc}
  chmod 755 "$d" "$d"/bin "$d"/etc "$d"/tmp
  : >"$d/bin/busybox"
  chmod 644 "$d/bin/busybox"
  printf '%s\n' "$d"
}

run_engine() { # run_engine <out> <err> <engine-run args...>
  local out="$1" err="$2"; shift 2
  set +e
  "$BIN" engine-run "$@" >"$out" 2>"$err"
  RC=$?
  set -e
}

# --- E1: exit code passthrough; also the preflight/SKIP probe ---------------
R="$(new_rootfs e1)"
run_engine "$TMP/e1.out" "$TMP/e1.err" \
  --rootfs "$R" --mount "$BUSYBOX:/bin/busybox" -- /bin/busybox sh -c 'exit 42'
# `engine-run` reports a failed preflight as exit 125 with an
# "engine-run: ..." stderr line (the "preflight failed" wording only exists
# on the long-lived `engine serve` path). The three environment-precondition
# messages come from nsrun::preflight (real root / mount_setattr(2) / /proc):
# none of them is a conformance break, so SKIP instead of FAIL.
if [[ "$RC" -eq 125 ]] && \
   grep -qE 'mount_setattr\(2\)|requires real root|/proc is required' "$TMP/e1.err"; then
  skip "$(head -1 "$TMP/e1.err")"
fi
check_rc "E1 exit 42" 42 "$RC"

# --- E2: exec failure is 127, and the child error text reaches the parent ----
R="$(new_rootfs e2)"
run_engine "$TMP/e2.out" "$TMP/e2.err" \
  --rootfs "$R" --mount "$BUSYBOX:/bin/busybox" -- /bin/nope
check_rc "E2 missing binary" 127 "$RC"
check_has "E2 exec error surfaced" "No such file" "$TMP/e2.err"

# --- E3: exec a directory is 126 -------------------------------------------
R="$(new_rootfs e3)"
run_engine "$TMP/e3.out" "$TMP/e3.err" \
  --rootfs "$R" --mount "$BUSYBOX:/bin/busybox" -- /
check_rc "E3 exec directory" 126 "$RC"

# --- E4: uid/gid/env/hostname are applied before exec ----------------------
R="$(new_rootfs e4)"
run_engine "$TMP/e4.out" "$TMP/e4.err" \
  --rootfs "$R" --hostname engine-e4 --uid 1234 --gid 2345 --env FOO=bar \
  --mount "$BUSYBOX:/bin/busybox" -- \
  /bin/busybox sh -c 'echo "uid=$(busybox id -u) gid=$(busybox id -g) foo=$FOO host=$(busybox hostname)"'
check_rc "E4 identity run" 0 "$RC"
check_has "E4 uid/gid/env/hostname" "uid=1234 gid=2345 foo=bar host=engine-e4" "$TMP/e4.out"

# --- E5: the exec'd process is PID 1, caps are bounded, no_new_privs ------
R="$(new_rootfs e5)"
run_engine "$TMP/e5.out" "$TMP/e5.err" \
  --rootfs "$R" --mount "$BUSYBOX:/bin/busybox" -- /bin/busybox cat /proc/self/status
check_rc "E5 status read" 0 "$RC"
if awk '$1 == "NSpid:" && $2 == "1" { found = 1 } END { exit !found }' "$TMP/e5.out"; then
  ok "E5 PID namespace (NSpid: 1)"
else
  bad "E5 PID namespace (NSpid is not 1)"
fi
check_has "E5 cap bounding set dropped" "CapBnd:	00000000a80425fb" "$TMP/e5.out"
check_has "E5 no_new_privs" "NoNewPrivs:	1" "$TMP/e5.out"

# --- E6: :ro bind mounts are read-only; rw dirs write through -------------
R="$(new_rootfs e6)"
mkdir -p "$R/ro"
printf 'ro-secret\n' >"$R/ro/secret"
printf 'etc-secret\n' >"$R/etc/rofile"
RW_SRC="$TMP/rw-src"
mkdir -p "$RW_SRC"
run_engine "$TMP/e6-ro.out" "$TMP/e6-ro.err" \
  --rootfs "$R" --mount "$BUSYBOX:/bin/busybox" \
  --mount "$R/etc/rofile:/etc/rofile:ro" --mount "$R/ro:/ro:ro" -- \
  /bin/busybox sh -c 'cat /etc/rofile /ro/secret; echo x >> /ro/secret'
check_rc "E6 read-only bind rejects writes" 1 "$RC"
check_has "E6 read-only bind still readable" "ro-secret" "$TMP/e6-ro.out"
check_has "E6 read-only file bind readable" "etc-secret" "$TMP/e6-ro.out"
run_engine "$TMP/e6-rw.out" "$TMP/e6-rw.err" \
  --rootfs "$R" --mount "$BUSYBOX:/bin/busybox" --mount "$RW_SRC:/rw" -- \
  /bin/busybox sh -c 'echo data > /rw/out'
check_rc "E6 read-write bind accepts writes" 0 "$RC"
if [[ -f "$RW_SRC/out" ]] && [[ "$(cat "$RW_SRC/out")" == "data" ]]; then
  ok "E6 read-write bind writes through to the host"
else
  bad "E6 read-write bind did not reach the host"
fi

# --- E7: cgroup v2 limits are applied and the cgroup is removed -----------
if [[ -f /sys/fs/cgroup/cgroup.controllers ]]; then
  R="$(new_rootfs e7)"
  CG="selftest-$$-$RANDOM"
  run_engine "$TMP/e7.out" "$TMP/e7.err" \
    --rootfs "$R" --mount "$BUSYBOX:/bin/busybox" --cgroup "$CG" \
    --pids-max 16 --memory-max 33554432 --cpu-max 50000/100000 -- \
    /bin/busybox sh -c 'cat /sys/fs/cgroup/pids.max /sys/fs/cgroup/memory.max /sys/fs/cgroup/cpu.max'
  check_rc "E7 cgroup run" 0 "$RC"
  check_has "E7 pids.max" "16" "$TMP/e7.out"
  check_has "E7 memory.max" "33554432" "$TMP/e7.out"
  check_has "E7 cpu.max" "50000 100000" "$TMP/e7.out"
  if [[ ! -d "/sys/fs/cgroup/init-pro/$CG" ]]; then
    ok "E7 cgroup removed after exit"
  else
    bad "E7 cgroup leaked at /sys/fs/cgroup/init-pro/$CG"
    rmdir "/sys/fs/cgroup/init-pro/$CG" 2>/dev/null || true
  fi
else
  echo "SKIP E7 (cgroup v2 is not mounted)"
fi

# --- E8: --new-net is a fresh network namespace ---------------------------
R="$(new_rootfs e8)"
HOST_NET="$(readlink /proc/self/ns/net)"
run_engine "$TMP/e8.out" "$TMP/e8.err" \
  --rootfs "$R" --new-net --mount "$BUSYBOX:/bin/busybox" -- \
  /bin/busybox readlink /proc/self/ns/net
check_rc "E8 new-net run" 0 "$RC"
CHILD_NET="$(tr -d '[:space:]' <"$TMP/e8.out")"
if [[ -n "$CHILD_NET" && "$CHILD_NET" != "$HOST_NET" && "$CHILD_NET" == net:* ]]; then
  ok "E8 --new-net ($HOST_NET -> $CHILD_NET)"
else
  bad "E8 --new-net (host [$HOST_NET] child [$CHILD_NET])"
fi

# --- E12: --host-root keeps the HOST root (T4.15, Q60) --------------------
# No --rootfs, no pivot_root, no overlay, no NEWNET: the child runs on
# the private NEWNS copy of the host mount table (fresh /proc for the new
# PID ns; host /dev and /sys untouched), and the HOST network namespace
# stays. The marker on the real host / is VISIBLE (the opposite of the
# E2/H2 pivot isolation).
E12_MARKER=/e12t415-marker
mkdir -p "$E12_MARKER"; echo hd-secret >"$E12_MARKER/secret"
HOST_PID_NS="$(readlink /proc/self/ns/pid)"
run_engine "$TMP/e12.out" "$TMP/e12.err" \
  --host-root -- \
  "$BUSYBOX" sh -c "echo M=\$(cat $E12_MARKER/secret); \
echo D=\$(test -c /dev/null && echo char); \
echo P=\$(readlink /proc/self/ns/pid); \
echo N=\$(readlink /proc/self/ns/net); \
echo S=\$(grep -c 'shared:' /proc/self/mountinfo)"
rm -rf "$E12_MARKER"
check_rc "E12 host-root run" 0 "$RC"
check_has "E12 host marker visible through the host root" "M=hd-secret" "$TMP/e12.out"
check_has "E12 host /dev visible" "D=char" "$TMP/e12.out"
E12_PID_NS="$(sed -n 's/^P=//p' "$TMP/e12.out" | tr -d '[:space:]')"
if [[ -n "$E12_PID_NS" && "$E12_PID_NS" != "$HOST_PID_NS" ]]; then
  ok "E12 fresh PID namespace ($HOST_PID_NS -> $E12_PID_NS)"
else
  bad "E12 PID namespace (host [$HOST_PID_NS] child [$E12_PID_NS])"
fi
E12_NET="$(sed -n 's/^N=//p' "$TMP/e12.out" | tr -d '[:space:]')"
if [[ -n "$E12_NET" && "$E12_NET" == "$HOST_NET" ]]; then
  ok "E12 stays in the host network namespace (no NEWNET)"
else
  bad "E12 host netns (host [$HOST_NET] child [$E12_NET])"
fi
check_has "E12 mount tree is private (no shared: tags)" "S=0" "$TMP/e12.out"
# Q60 strictness: --host-root refuses net flags — the host network stays.
run_engine "$TMP/e12b.out" "$TMP/e12b.err" --host-root --new-net -- true
if [[ "$RC" -ne 0 ]] && grep -q "host network" "$TMP/e12b.err"; then
  ok "E12b --host-root rejects --new-net"
else
  bad "E12b --host-root + --new-net (rc=$RC, err: $(head -1 "$TMP/e12b.err"))"
fi

# --- E9-E11: netns join + signal-death exits -------------------------------
# These need pgrep to find the container child (the single direct child
# of a running engine-run). Without pgrep the cases SKIP, not FAIL.
if ! command -v pgrep >/dev/null; then
  echo "SKIP E9-E11 (pgrep is not available)"
else
  # holder_pid <engine-run-pid>: poll until nsrun::spawn forked the child.
  holder_pid() {
    local i p
    for i in $(seq 100); do
      p="$(pgrep -P "$1" 2>/dev/null | head -n1)"
      if [[ -n "$p" ]]; then
        printf '%s\n' "$p"
        return 0
      fi
      kill -0 "$1" 2>/dev/null || break
      sleep 0.05
    done
    return 1
  }

  # signal_exit <label> <signo> <want> <argv...>: start a fresh-netns
  # container, kill its PID 1 from outside. A pid-ns init only receives
  # an ancestor's SIGTERM if it trapped it (SIGKILL always lands), so
  # the unhandled case asserts 128+signo and the graceful one the trap
  # code.
  signal_exit() {
    local label="$1" signo="$2" want="$3"; shift 3
    local r epid cpid
    r="$(new_rootfs "sig$signo")"
    "$BIN" engine-run --rootfs "$r" --new-net --mount "$BUSYBOX:/bin/busybox" \
      -- "$@" >"$TMP/sig$signo.out" 2>"$TMP/sig$signo.err" &
    epid=$!
    if cpid="$(holder_pid "$epid")"; then
      kill "-$signo" "$cpid"
      set +e
      wait "$epid"
      RC=$?
      set -e
      check_rc "$label" "$want" "$RC"
    else
      bad "$label (holder container never started)"
      pkill -P "$epid" 2>/dev/null || true
      kill "$epid" 2>/dev/null || true
      set +e
      wait "$epid" 2>/dev/null
      set -e
    fi
  }

  # E9: a second engine-run joins the first container's netns verbatim.
  R="$(new_rootfs e9)"
  "$BIN" engine-run --rootfs "$R" --new-net --mount "$BUSYBOX:/bin/busybox" \
    -- /bin/busybox sleep 30 >"$TMP/e9.hold.out" 2>"$TMP/e9.hold.err" &
  HOLD=$!
  if CPID="$(holder_pid "$HOLD")"; then
    HOLDER_NET="$(readlink "/proc/$CPID/ns/net")"
    R="$(new_rootfs e9b)"
    run_engine "$TMP/e9.out" "$TMP/e9.err" \
      --rootfs "$R" --netns "/proc/$CPID/ns/net" --mount "$BUSYBOX:/bin/busybox" -- \
      /bin/busybox readlink /proc/self/ns/net
    check_rc "E9 netns join run" 0 "$RC"
    JOIN_NET="$(tr -d '[:space:]' <"$TMP/e9.out")"
    if [[ -n "$JOIN_NET" && "$JOIN_NET" == "$HOLDER_NET" ]]; then
      ok "E9 --netns joined the holder's netns"
    else
      bad "E9 --netns (holder [$HOLDER_NET] joiner [$JOIN_NET])"
    fi
    kill "$CPID" 2>/dev/null || true
    set +e
    wait "$HOLD" 2>/dev/null
    set -e
  else
    bad "E9 holder container never started"
    pkill -P "$HOLD" 2>/dev/null || true
    kill "$HOLD" 2>/dev/null || true
    set +e
    wait "$HOLD" 2>/dev/null
    set -e
  fi

  signal_exit "E10 SIGKILL death -> 137" 9 137 /bin/busybox sleep 30
  signal_exit "E11 SIGTERM runs the trapping init (exit 7)" 15 7 \
    /bin/busybox sh -c 'trap "exit 7" TERM; while :; do sleep 1; done'
fi

echo
echo "engine-selftest: ${PASS} passed / ${FAIL} failed"
[[ "$FAIL" -eq 0 ]]
