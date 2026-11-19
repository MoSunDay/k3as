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

DD="$(mktemp -d /tmp/init-pro-builtin-hostroot-e2e.XXXXXX)"
KUBECTL="$DD/kubectl"; ln -sf "$BIN" "$KUBECTL"
SRV_PID=""; AGENT_PID=""
bimg_init_traps
# H2 puts a marker on the REAL host / — chained teardown removes it.
h2_marker=/h2t414-marker
h_cleanup() { rm -rf "$h2_marker" 2>/dev/null || true; bimg_cleanup; }
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

# --- H8: the image chain regresses green under --builtin-rootfs image --------
# The child sources the same lib and would inherit our exported
# BIMG_ROOTFS_MODE=host — force the image-chain boot for the regression.
if BIMG_ROOTFS_MODE=image "$SCRIPT_DIR/builtin-image-e2e.sh"; then
  bimg_ok "H8 builtin-image-e2e (I1-I8) green under --builtin-rootfs image"
else
  bimg_fail "H8 builtin-image-e2e regressed under --builtin-rootfs image"
fi

echo "builtin-hostroot-e2e: H1-H8 passed (host-rootfs default, Q55)"
