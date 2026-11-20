#!/usr/bin/env bash
# T4.14 (Q55): the built-in engine's HOST-ROOTFS default — the agent boots
# WITHOUT --builtin-rootfs (i.e. the Q55 default `host`), pods take host
# directories as their rootfs (absolute path or a name staged under
# <data_dir>/engine/images/<name>), and `init-pro.io/host-mounts` binds
# host directories into the container. The Q53 image chain stays reachable
# per pod via `init-pro.io/rootfs: image` and engine-wide via the flag
# (re-proven by H8 rerunning builtin-image-e2e.sh, which boots
# `--builtin-rootfs image`).
#
# Cases:
#   H1 both host-rootfs spellings: (a) absolute image path $DD/hroot
#      (fabricated busybox rootfs) -> pod runs /busybox and logs a marker;
#      (b) staged name hroot-staged (fabricated under the agent's
#      engine/images dir, beats the legacy images/ fallback) -> same shape.
#   H2 NEWNS isolation: a marker dir created on the REAL host / is
#      invisible inside a staged-rootfs container (pivot_root + lazy
#      detach of the old tree).
#   H3 host-mounts :rw: a write inside /data lands on the host dir.
#   H4 host-mounts :ro: `busybox test -w /data` fails (mount_setattr
#      EROFS bind) -> exitCode 1, restartPolicy Never -> phase Failed.
#   H5 missing host-mount source: pod stays Pending; the agent log carries
#      the create-time pre-check error and the pod gets a Failed action
#      event with the actionable text (bind is the permission verdict;
#      existence is the one pre-check, Q55).
#   H6 missing rootfs name in host mode: EnsureImage is presence-only, so
#      the pod stays Pending with an actionable Failed event naming the
#      staged dir AND the init-pro.io/rootfs escape hatch.
#   H7 flagship lifecycle: full host access through host-mounts —
#      /host=/ binds the host tree (MS_REC, :ro) into the container, and
#      /workspace binds the real /workspace when the environment has one
#      (e.g. an anyhome NFS mount.nfs mount — else a scratch dir):
#      Running, logs marker, kubectl exec writes through the bind + reads
#      the host tree, delete -> gone. (The literal image=/ spelling is
#      NOT supported: nsrun's pre-pivot fresh sysfs mount collides with
#      the copied tree — clean EBUSY failure, Q55.)
#   H8 regression: builtin-image-e2e.sh reruns green under
#      `--builtin-rootfs image` (I1-I8 unchanged on the opt-in chain).
#   T4.15 (Q60) host-direct pods — the literal `init-pro.io/rootfs: /`
#   annotation rides the strict triple (builtin engine + host rootfs
#   mode + `/`): the image is pure wire metadata (a BOGUS reference with
#      imagePullPolicy: Never — no image action may run):
#   HD1/HD2 selection + direct root: the host marker on the REAL / is
#      VISIBLE through the host root (the H2 inverse) and the pod runs.
#   HD3 namespaces: host netns stays (no NEWNET), fresh PID ns through
#      /proc, private mount propagation (no shared: tags).
#   HD4 existing controls: a precreated fake device under host /dev is
#      visible; :ro host-mounts still refuse writes (EROFS -> Failed);
#      a memory limit lands in the container cgroup (init-pro/<id>/
#      memory.max — /sys stays the TRUE host view, Q60).
#   HD5 explicit failure: agent restarted `--builtin-rootfs image` breaks
#      the triple — the `/` pod NEVER runs (Failed/Pending + the
#      "host-direct refused" text), no containerd fallback, agent alive.
#
# Preconditions: PRE-BUILT binary, real root, static busybox, curl +
# python3. Missing inputs => "SKIP builtin-hostroot-e2e:" + exit 0 (Q25);
# a violated assertion is a hard FAIL.
#
# Usage: scripts/builtin-hostroot-e2e.sh
#   INIT_PRO_BIN=...      reuse a specific binary (default target/debug/init-pro)
#   INIT_PRO_BUSYBOX=...  override the static busybox (default /usr/bin/busybox)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${INIT_PRO_BIN:-$TARGET_DIR/debug/init-pro}"
BUSYBOX="${INIT_PRO_BUSYBOX:-/usr/bin/busybox}"

skip() { echo "SKIP builtin-hostroot-e2e: $*"; exit 0; }

[[ -x "$BIN" ]] || skip "no binary at $BIN (build first)"
[[ -x "$BUSYBOX" ]] || skip "no static busybox at $BUSYBOX"
command -v curl >/dev/null || skip "curl missing"
command -v python3 >/dev/null || skip "python3 missing"
[[ "$(id -u)" == "0" ]] || skip "needs real root (pivot_root/mount)"

# shellcheck source=scripts/lib/builtin-image-lib.sh
source "$SCRIPT_DIR/lib/builtin-image-lib.sh"

# T4.14 (Q55): boot the agent on the HOST default (no --builtin-rootfs).
export BIMG_ROOTFS_MODE=host

DD=/tmp/hdfix; rm -rf /tmp/hdfix; mkdir -p /tmp/hdfix
KUBECTL="$DD/kubectl"; ln -sf "$BIN" "$KUBECTL"
SRV_PID=""; AGENT_PID=""
bimg_init_traps
# H2 + HD1 put markers on the REAL host / and /dev — chained teardown
# removes them (the device node is the HD4 fixture, T4.15/Q60).
h2_marker=/h2t414-marker
hd_marker=/hdt415-marker
hd_dev=/dev/k3as-hd4
h_cleanup() {
  rm -rf "$h2_marker" "$hd_marker" 2>/dev/null || true
  rm -f "$hd_dev" 2>/dev/null || true
  bimg_cleanup
}
trap h_cleanup EXIT

API_PORT="$(pick_port)"
REG_PORT="$(pick_port)"
BASE="http://127.0.0.1:${API_PORT}"
REG="http://127.0.0.1:${REG_PORT}"
NODE="hostroot-node"
SRV_LOG="$DD/server.log"; AGENT_LOG="$DD/agent.log"
boot_cluster
bimg_ok "cluster up (server + builtin agent, ROOTFS MODE = host default)"

# --- helpers ---------------------------------------------------------------
# A fabricated host rootfs: static busybox at /busybox, nothing else.
make_hroot() { # DIR
  mkdir -p "$1"
  cp "$BUSYBOX" "$1/busybox"
  chmod 755 "$1/busybox"
}

post_pod_full() { # NAME POD_JSON -> POST status
  curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' \
    --data-binary "$2" "$BASE/api/v1/namespaces/default/pods"
}

pod_json() { # NAME IMAGE CMD_JSON [ANN_KEY] [ANN_VAL] [RESTART] -> body
  # Terminating pods default to Never (phase Succeeded/Failed on exit);
  # long-runners (H7) pass Always explicitly.
  local ann="{}" restart="Never"
  [[ -n "${4:-}" ]] && ann="$(python3 -c 'import json,sys; print(json.dumps({sys.argv[1]: sys.argv[2]}))' "$4" "$5")"
  [[ -n "${6:-}" ]] && restart="$6"
  python3 - "$1" "$2" "$3" "$ann" "$restart" <<'PY'
import json, sys
name, image, cmd, ann, restart = sys.argv[1:6]
print(json.dumps({
    "apiVersion": "v1", "kind": "Pod",
    "metadata": {"name": name, "namespace": "default", "annotations": json.loads(ann)},
    "spec": {"restartPolicy": restart,
             "containers": [{"name": "c", "image": image, "command": json.loads(cmd)}]},
}))
PY
}

pod_events() { # NAME -> "reason | message" lines for the pod
  curl -s "$BASE/api/v1/namespaces/default/events" | python3 -c '
import json, sys
try:
    evs = json.load(sys.stdin).get("items", [])
except Exception:
    sys.exit(0)
for e in evs:
    if e.get("involvedObject", {}).get("name") == sys.argv[1]:
        print(e.get("reason", ""), "|", e.get("message", ""))
' "$1" 2>/dev/null || true
}

wait_pod_events() { # NAME PATTERN TENTHS -> matching event lines (bounded)
  local out=""
  for _ in $(seq 1 "$3"); do
    out="$(pod_events "$1")"
    grep -q "$2" <<<"$out" && break
    sleep 0.5
  done
  echo "$out"
}

exit_code() { # NAME -> first container exit code ("" while running)
  curl -s "$BASE/api/v1/namespaces/default/pods/$1" | python3 -c '
import json, sys
try:
    cs = json.load(sys.stdin)["status"].get("containerStatuses", [{}])[0]
    for k in ("lastState", "state"):
        t = cs.get(k, {}).get("terminated")
        if t is not None:
            print(t.get("exitCode", ""))
            break
    else:
        print("")
except Exception:
    print("")
' 2>/dev/null || true
}

run_marker_pod() { # NAME IMAGE CMD_JSON EXPECTED_MARK [ANN_KEY] [ANN_VAL] [RESTART]
  local code phase logs
  code="$(post_pod_full "$1" "$(pod_json "$1" "$2" "$3" "${5:-}" "${6:-}" "${7:-}")")"
  [[ "$code" == "201" ]] || bimg_fail "$1 pod POST -> $code"
  phase="$(wait_phase "$1" Succeeded 120)"
  [[ "$phase" == "Succeeded" ]] \
    || bimg_fail "$1 never Succeeded (phase=${phase:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
  logs="$(wait_logs "$1" "$4" 60)"
  grep -q "$4" <<<"$logs" || bimg_fail "$1 logs missing '$4' (got: ${logs:-<empty>})"
}

# --- H1: both host-rootfs spellings ----------------------------------------
make_hroot "$DD/hroot"
run_marker_pod h1-abs "$DD/hroot" '["/busybox","echo","H1-ABS-OK"]' "H1-ABS-OK"
bimg_ok "H1a absolute image path runs a host dir as rootfs"

STAGED="$DD/agent/engine/images/hroot-staged"
make_hroot "$STAGED"
run_marker_pod h1-staged "hroot-staged" '["/busybox","echo","H1-STAGED-OK"]' "H1-STAGED-OK"
bimg_ok "H1b staged name <data>/engine/images/<name> resolves as rootfs"

# --- H2: NEWNS isolation (host / marker invisible) --------------------------
mkdir -p "$h2_marker"; echo host-secret >"$h2_marker/secret"
run_marker_pod h2-iso "hroot-staged" \
  '["/busybox","sh","-c","test -d /h2t414-marker && echo LEAK || echo H2-ISO-OK"]' "H2-ISO-OK"
bimg_ok "H2 host-root marker on real / invisible in the staged-rootfs container"
rm -rf "$h2_marker"

# --- H3: host-mounts rw write lands on the host -----------------------------
mkdir -p "$DD/h3data"
run_marker_pod h3-rw "$DD/hroot" \
  '["/busybox","sh","-c","echo H3-WROTE > /data/h3.txt && echo H3-DONE"]' "H3-DONE" \
  init-pro.io/host-mounts "/data=$DD/h3data:rw"
grep -q "H3-WROTE" "$DD/h3data/h3.txt" 2>/dev/null \
  || bimg_fail "H3 rw host-mount write never landed on the host side"
bimg_ok "H3 :rw bind write visible on the host"

# --- H4: host-mounts ro is EROFS (write refused -> exit 1, Never -> Failed) -
# busybox `test -w` answers YES for root even on a ro bind (DAC_OVERRIDE in
# access(2)); the WRITE attempt is the ground truth (mount_setattr EROFS).
mkdir -p "$DD/h4data"; echo seed >"$DD/h4data/seed"
code="$(post_pod_full h4-ro "$(pod_json h4-ro "$DD/hroot" \
  '["/busybox","sh","-c","echo x > /data/w"]' init-pro.io/host-mounts "/data=$DD/h4data:ro" Never)")"
[[ "$code" == "201" ]] || bimg_fail "H4 pod POST -> $code"
PHASE="$(wait_phase h4-ro Failed 120)"
[[ "$PHASE" == "Failed" ]] || bimg_fail "H4 :ro pod should go Failed (write refused), got ${PHASE:-none}"
[[ "$(exit_code h4-ro)" == "1" ]] || bimg_fail "H4 exitCode expected 1, got '$(exit_code h4-ro)'"
LOGS="$("$KUBECTL" logs h4-ro --server "$BASE" 2>/dev/null || true)"
grep -q "Read-only file system" <<<"$LOGS" \
  || bimg_fail "H4 logs lack the EROFS marker (got: ${LOGS:-<empty>})"
bimg_ok "H4 :ro bind refuses writes (EROFS -> exit 1 -> phase Failed)"

# --- H5: missing host-mount source -> Pending + log + event -----------------
mkdir -p "$DD/h5"
code="$(post_pod_full h5-gone "$(pod_json h5-gone "$DD/hroot" \
  '["/busybox","echo","never"]' init-pro.io/host-mounts "/gone=$DD/h5-missing:rw")")"
[[ "$code" == "201" ]] || bimg_fail "H5 pod POST -> $code"
sleep 8
PHASE="$(pod_phase h5-gone)"
[[ "$PHASE" == "Pending" ]] || bimg_fail "H5 missing-source pod should stay Pending, got ${PHASE:-none}"
EV="$(wait_pod_events h5-gone "does not exist on the node" 40)"
grep -q "Failed" <<<"$EV" \
  || bimg_fail "H5 no Failed action event (events: $(echo "$EV" | head -3))"
grep -q "does not exist on the node" <<<"$EV" \
  || bimg_fail "H5 event lacks the actionable text"
grep -q "does not exist on the node" "$AGENT_LOG" \
  || bimg_fail "H5 agent log lacks the pre-check error"
bimg_ok "H5 missing mount source: Pending + log + Failed action event"

# --- H6: missing rootfs name (host mode EnsureImage) ------------------------
code="$(post_pod_full h6-missing "$(pod_json h6-missing "nosuch-rootfs-t414" \
  '["/busybox","echo","never"]')")"
[[ "$code" == "201" ]] || bimg_fail "H6 pod POST -> $code"
sleep 8
PHASE="$(pod_phase h6-missing)"
[[ "$PHASE" == "Pending" ]] || bimg_fail "H6 missing-rootfs pod should stay Pending, got ${PHASE:-none}"
EV="$(wait_pod_events h6-missing "Failed" 40)"
grep -q "Failed" <<<"$EV" || bimg_fail "H6 no Failed event (events: $(echo "$EV" | head -3))"
grep -q "engine/images" <<<"$EV" || bimg_fail "H6 event lacks the staged-dir remedy"
grep -q "init-pro.io/rootfs" <<<"$EV" || bimg_fail "H6 event lacks the annotation escape hatch"
grep -q "no root directory" "$AGENT_LOG" || bimg_fail "H6 agent log lacks the EnsureImage error"
kill -0 "$AGENT_PID" 2>/dev/null || bimg_fail "H6 agent died on the rootfs miss"
bimg_ok "H6 missing rootfs name: Pending + actionable Failed event; agent alive"

# --- H7: flagship lifecycle with the whole host tree on demand --------------
if [[ -d /workspace ]]; then H7WS=/workspace; else H7WS="$DD/h7ws"; mkdir -p "$H7WS"; fi
code="$(post_pod_full h7-host "$(pod_json h7-host "$DD/hroot" \
  '["/busybox","sh","-c","echo H7-UP; /busybox sleep 300"]' \
  init-pro.io/host-mounts "/workspace=$H7WS:rw,/host=/:ro")")"
[[ "$code" == "201" ]] || bimg_fail "H7 pod POST -> $code"
PHASE="$(wait_phase h7-host Running 120)"
[[ "$PHASE" == "Running" ]] \
  || bimg_fail "H7 host-root pod never Running (phase=${PHASE:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
LOGS="$(wait_logs h7-host "H7-UP" 60)"
grep -q "H7-UP" <<<"$LOGS" || bimg_fail "H7 logs missing the marker (got: ${LOGS:-<empty>})"
"$KUBECTL" exec h7-host --server "$BASE" -- /busybox sh -c 'echo H7-EXEC > /workspace/exec.txt; test -f /host/etc/hostname' \
  || bimg_fail "H7 kubectl exec failed (write /workspace or read /host)"
for _ in $(seq 1 20); do [[ -f "$H7WS/exec.txt" ]] && break; sleep 0.5; done
grep -q "H7-EXEC" "$H7WS/exec.txt" 2>/dev/null \
  || bimg_fail "H7 exec write never landed in the host workspace"
delete_pod h7-host
bimg_ok "H7 host-access pod: Running + logs + exec + /workspace ($H7WS) + /host tree, clean delete"

# --- HD1-HD5 (T4.15, Q60): host-direct pods on the literal host root -------
# Helpers: the host-direct pod body (bogus image = pure wire metadata,
# imagePullPolicy Never so NO image action runs) and a busybox argv whose
# binary lives on the HOST root ($BUSYBOX is the host path — no rootfs is
# fabricated for these pods).
hd_cmd() { # SH_SCRIPT -> JSON argv ["<host busybox>", "sh", "-c", SCRIPT]
  python3 -c 'import json,sys; print(json.dumps([sys.argv[1], "sh", "-c", sys.argv[2]]))' \
    "$BUSYBOX" "$1"
}

hd_pod_json() { # NAME CMD_JSON [HOST_MOUNTS_SPEC] [LIMITS_JSON] -> pod body
  python3 - "$1" "$2" "${3:-}" "${4:-}" <<'PY'
import json, sys
name, cmd, hm, limits = sys.argv[1:5]
container = {
    "name": "c",
    "image": "registry.invalid/no-such/image:never",
    "imagePullPolicy": "Never",
    "command": json.loads(cmd),
}
if limits:
    container["resources"] = {"limits": json.loads(limits)}
ann = {"init-pro.io/rootfs": "/"}
if hm:
    ann["init-pro.io/host-mounts"] = hm
print(json.dumps({
    "apiVersion": "v1", "kind": "Pod",
    "metadata": {"name": name, "namespace": "default", "annotations": ann},
    "spec": {"restartPolicy": "Never", "containers": [container]},
}))
PY
}

# hd_run_pod NAME MARKER [HOST_MOUNTS_SPEC] [LIMITS_JSON] [CMD_JSON]:
# post, wait Succeeded, wait logs; leaves the logs in HD_LOGS.
hd_run_pod() {
  local code cmd="${5:-$(hd_cmd "echo $2")}"
  code="$(post_pod_full "$1" "$(hd_pod_json "$1" "$cmd" "${3:-}" "${4:-}")")"
  [[ "$code" == "201" ]] || bimg_fail "$1 pod POST -> $code"
  local phase; phase="$(wait_phase "$1" Succeeded 120)"
  [[ "$phase" == "Succeeded" ]] \
    || bimg_fail "$1 never Succeeded (phase=${phase:-none}; agent log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
  HD_LOGS="$(wait_logs "$1" "$2" 60)"
  grep -q "$2" <<<"$HD_LOGS" || bimg_fail "$1 logs missing '$2' (got: ${HD_LOGS:-<empty>})"
}

# HD1/HD2: the strict triple selects host-direct; the host marker on the
# REAL / is VISIBLE (H2's inverse: staged-rootfs pods cannot see it) and
# the bogus image never mattered (no image action ran).
mkdir -p "$hd_marker"; echo hd-root-secret >"$hd_marker/secret"
hd_run_pod hd1-host "HD1-UP" "" "" \
  "$(hd_cmd 'echo HD1-UP; echo M=$(cat /hdt415-marker/secret)')"
grep -q "M=hd-root-secret" <<<"$HD_LOGS" \
  || bimg_fail "HD1 host marker invisible through the host root (got: ${HD_LOGS:-<empty>})"
bimg_ok "HD1/HD2 host-direct pod runs; bogus image + Never => no image action; host / marker visible"

# HD3: namespaces — the HOST netns stays (no NEWNET, no join), the PID
# namespace is fresh (visible through the fresh /proc), and the private
# NEWNS copy has no shared: propagation tags.
HOST_NET_NS="$(readlink /proc/self/ns/net)"
HOST_PID_NS="$(readlink /proc/self/ns/pid)"
hd_run_pod hd3-ns "N=" "" "" \
  "$(hd_cmd "echo N=\$(readlink /proc/self/ns/net); echo P=\$(readlink /proc/self/ns/pid); echo S=\$(grep -c 'shared:' /proc/self/mountinfo)")"
HD3_NET="$(grep -o 'N=net:\[[0-9]*\]' <<<"$HD_LOGS" | head -1 | cut -d= -f2)"
HD3_PID="$(grep -o 'P=pid:\[[0-9]*\]' <<<"$HD_LOGS" | head -1 | cut -d= -f2)"
[[ "$HD3_NET" == "$HOST_NET_NS" ]] \
  || bimg_fail "HD3 must stay in the host netns (host [$HOST_NET_NS] pod [$HD3_NET])"
[[ -n "$HD3_PID" && "$HD3_PID" != "$HOST_PID_NS" ]] \
  || bimg_fail "HD3 needs a fresh PID ns (host [$HOST_PID_NS] pod [$HD3_PID])"
grep -q "S=0" <<<"$HD_LOGS" || bimg_fail "HD3 mountinfo still has shared: tags (${HD_LOGS})"
bimg_ok "HD3 host netns kept, fresh PID ns, private mount tree"

# HD4: the existing controls hold on the host root — a precreated fake
# device node under the host /dev is visible; :ro host-mounts still
# refuse writes; a memory limit lands in the container cgroup.
if ! command -v mknod >/dev/null; then
  bimg_ok "HD4a SKIP (no mknod on this host)"
else
  rm -f "$hd_dev"; mknod "$hd_dev" c 1 3 || bimg_fail "HD4 mknod $hd_dev failed"
  hd_run_pod hd4-dev "HD4-DEV-OK" "" "" \
    "$(hd_cmd "test -c $hd_dev && echo HD4-DEV-OK")"
  bimg_ok "HD4a precreated device $hd_dev visible through host /dev"
fi
mkdir -p "$DD/hddata"; echo seed >"$DD/hddata/seed"
code="$(post_pod_full hd4-ro "$(hd_pod_json hd4-ro "$(hd_cmd 'echo x > /data/w')" "/data=$DD/hddata:ro")")"
[[ "$code" == "201" ]] || bimg_fail "HD4b pod POST -> $code"
PHASE="$(wait_phase hd4-ro Failed 120)"
[[ "$PHASE" == "Failed" ]] || bimg_fail "HD4b :ro write should go Failed, got ${PHASE:-none}"
[[ "$(exit_code hd4-ro)" == "1" ]] || bimg_fail "HD4b exitCode expected 1, got '$(exit_code hd4-ro)'"
HD4B_LOGS="$("$KUBECTL" logs hd4-ro --server "$BASE" 2>/dev/null || true)"
grep -q "Read-only file system" <<<"$HD4B_LOGS" \
  || bimg_fail "HD4b logs lack the EROFS marker (got: ${HD4B_LOGS:-<empty>})"
# HD4c memory limit: Q60 keeps host /sys untouched, so the host root shows
# the TRUE host cgroupfs — whose v2 ROOT has no memory.max (ENOENT; the
# pivot path differs because nsrun mounts a fresh cgroupfs there). The
# container's own cgroup is a CHILD of nsrun's init-pro parent, named
# after the container id (engine convert.rs sanitize) — read it from the
# pod's view of the host tree.
hd_run_pod hd4-mem "MEM=" "" '{"memory":"16Mi"}' \
  "$(hd_cmd 'for f in /sys/fs/cgroup/init-pro/*/memory.max; do echo MEM=$(cat $f 2>/dev/null)@$f; done')"
grep -q "MEM=16777216" <<<"$HD_LOGS" \
  || bimg_fail "HD4c the 16Mi cap must land in the init-pro container cgroup (got: ${HD_LOGS})"
bimg_ok "HD4 host /dev visible; :ro bind EROFS -> Failed; memory limit applied"

# HD5: break the triple (agent on --builtin-rootfs image) — the `/` pod
# must NEVER run: no containerd fallback, an actionable refusal surfaced,
# and the agent stays alive. Restore the host default afterwards.
restart_agent --builtin-rootfs image
code="$(post_pod_full hd5-refused "$(hd_pod_json hd5-refused "$(hd_cmd 'echo never')")")"
[[ "$code" == "201" ]] || bimg_fail "HD5 pod POST -> $code"
sleep 8
PHASE="$(pod_phase hd5-refused)"
[[ "$PHASE" != "Running" && "$PHASE" != "Succeeded" ]] \
  || bimg_fail "HD5 broken-triple pod must never run (phase=$PHASE)"
HD5_EV="$(wait_pod_events hd5-refused "host-direct" 40)"
if ! grep -q "host-direct" <<<"$HD5_EV"; then
  grep -q "host-direct refused" "$AGENT_LOG" \
    || bimg_fail "HD5 neither events nor agent log carry the refusal (phase=$PHASE; log: $(tail -5 "$AGENT_LOG" | tr '\n' ' '))"
fi
kill -0 "$AGENT_PID" 2>/dev/null || bimg_fail "HD5 agent died on the refused pod"
delete_pod hd5-refused
restart_agent
bimg_ok "HD5 broken triple: pod refused (never runs), actionable text, agent alive, no fallback"

# --- H8: the image chain regresses green under --builtin-rootfs image --------
# The child sources the same lib and would inherit our exported
# BIMG_ROOTFS_MODE=host — force the image-chain boot for the regression.
if BIMG_ROOTFS_MODE=image "$SCRIPT_DIR/builtin-image-e2e.sh"; then
  bimg_ok "H8 builtin-image-e2e (I1-I8) green under --builtin-rootfs image"
else
  bimg_fail "H8 builtin-image-e2e regressed under --builtin-rootfs image"
fi

echo "builtin-hostroot-e2e: H1-H8 + HD1-HD5 passed (host-rootfs default Q55; host-direct Q60)"
