#!/usr/bin/env bash
# Sprint closure gate runner (S37 infra): the one-command Definition-of-Done
# battery that every sprint retrospective runs before marking a TODO done.
# Executes the project's DoD gate list in order, streams each gate's output,
# tees it to a per-gate log under a temp dir, and prints a summary table;
# exits nonzero iff any gate FAILs.
#
# Gate order (Sprint DoD):
#   1. cargo fmt --check   (workspace members only; see note below)
#   2. cargo build --workspace --locked
#   3. cargo test --workspace --locked   (pass/fail totals parsed + printed)
#   4. cargo clippy --workspace --all-targets -- -D warnings
#   5. scripts/golden-conformance.sh     (T0.6 merge gate, Q2)
#   6. domain e2e battery                (Q25 SKIP-not-fail doctrine)
#   7. scripts/cli-flag-parity-test.sh   (T0.4, Q9)
#   8. SSOT lock-step row                (manual; never fails the run)
#
# Notes:
# - CI orders `cargo fmt --all --check` BEFORE scripts/bootstrap-link-repos.sh
#   so fmt never reformats the foreign pinned trees. When link_repos IS
#   already materialized (the dev-machine case), `--all` would walk the path
#   deps and fail on their stable-vs-nightly rustfmt configs, so this gate
#   ALWAYS scopes to workspace members (`cargo metadata --no-deps`) — the
#   same package set CI's pre-bootstrap `--all` effectively covers.
# - Env overrides pass straight through to the children: INIT_PRO_BIN,
#   CARGO_TARGET_DIR, INIT_PRO_VENDOR_BIN, INIT_PRO_OFFLINE, INIT_PRO_VENDOR,
#   INIT_PRO_EMBED, ... (nothing is captured or overridden here).
# - e2e battery scripts each self-SKIP with exit 0 when their preconditions
#   are missing (no vendor bundle, no cc, no mount.nfs, no curl, no root);
#   a leading "SKIP " line in their output is recorded as a SKIP row that
#   counts as a pass (Q25: a missing build input is a configuration state,
#   not a conformance break). The one exception is service-traffic-e2e.sh:
#   it boots the agent+containerd stack (same recipe as local-up.sh, which
#   hard-requires root) and has NO root self-SKIP, so this runner gates the
#   root condition itself and records "SKIP (needs root)" as a pass.
# - Logs: every gate's full output is teed to $LOGDIR/<gate>.log; the dir is
#   kept when any gate FAILs (or KEEP_LOGS=1), removed otherwise.
#
# Usage: scripts/all-gates.sh [--no-e2e] [--list]
#   --no-e2e   skip gate 6 (the domain e2e battery)
#   --list     print the gate list without running anything
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

usage() {
  sed -n 's/^# Usage:/Usage:/p; s/^#   --/--/p' "$0" 2>/dev/null || true
  echo "See the header of $0 for the full gate list."
}

RUN_E2E=1
LIST_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --no-e2e) RUN_E2E=0 ;;
    --list)   LIST_ONLY=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $arg" >&2; usage >&2; exit 2 ;;
  esac
done

# The domain e2e battery (gate 6). Order: cheap server-only acceptance
# probes first, heavier full-stack suites last. Every entry is a genuine
# gate with self-SKIP semantics (see the per-script headers); interactive
# dev-loop helpers (local-up.sh), the historical t41-containerd-spike.sh
# (exit-1, no SKIP, superseded by golden G24-G26), stage-fresh-dir-test.sh
# (needs an INIT_PRO_EMBED=1 build), the image-builder helpers, and
# bootstrap-link-repos.sh (an environment bootstrap that runs BEFORE gates,
# never as one) are deliberately excluded.
BATTERY=(
  multicall-selftest.sh
  apiserver-discovery-parity-test.sh
  graceful-shutdown-test.sh
  durability-e2e.sh
  registry-e2e.sh
  anyhome-e2e.sh
  agent-exec-e2e.sh
  resources-e2e.sh
  service-traffic-e2e.sh
  flannel-e2e.sh
)
# Scripts that would FAIL (not SKIP) without root -> pre-gated here.
ROOT_GATED="service-traffic-e2e.sh"

GATE_NO=0
FAILS=0
SKIPS=0
ROW_NAME=()
ROW_STATUS=()
ROW_TIME=()

record() { # record <name> <status> <time>
  ROW_NAME+=("$1"); ROW_STATUS+=("$2"); ROW_TIME+=("$3")
}

# stream <log> <cmd...>: run cmd, tee combined output to <log>, set RC.
stream() {
  local log="$1"; shift
  set +e
  "$@" 2>&1 | tee "$log"
  RC="${PIPESTATUS[0]}"
  set -e
}

# gate <label> <log-stem> <cmd...>: a plain pass/fail gate.
gate() {
  local label="$1" stem="$2"; shift 2
  GATE_NO=$((GATE_NO + 1))
  local log="$LOGDIR/$stem.log" t0="$SECONDS"
  echo
  echo "== gate ${GATE_NO}: ${label} =="
  stream "$log" "$@"
  local dt=$((SECONDS - t0))
  if [[ "$RC" -eq 0 ]]; then
    record "$label" "PASS" "${dt}s"
  else
    record "$label" "FAIL (exit $RC)" "${dt}s"
    FAILS=$((FAILS + 1))
  fi
}

# e2e_row <script-name>: one battery entry. Exit 0 + leading "SKIP " line in
# the output => SKIP row (counts as pass, Q25); exit 0 otherwise => PASS.
e2e_row() {
  local s="$1"
  local label="  $s"
  local log="$LOGDIR/e2e-$s.log" t0="$SECONDS"
  echo
  echo "-- e2e: $s"
  if [[ " $ROOT_GATED " == *" $s "* && "$(id -u)" != "0" ]]; then
    echo "SKIP $s (needs root; the script has no root self-SKIP, pre-gated here)"
    record "$label" "SKIP (needs root)" "0s"
    SKIPS=$((SKIPS + 1))
    return 0
  fi
  stream "$log" "$SCRIPT_DIR/$s"
  local dt=$((SECONDS - t0))
  if [[ "$RC" -ne 0 ]]; then
    record "$label" "FAIL (exit $RC)" "${dt}s"
    FAILS=$((FAILS + 1))
    return 0
  fi
  local first why
  first="$(awk 'NF { print; exit }' "$log")"
  if [[ "$first" == SKIP* ]]; then
    # Compact reason: strip the "SKIP " tag, the script-name-ish prefix
    # detail beyond the first '(' or ';', then truncate for the table.
    why="${first#SKIP }"
    why="${why%%(*}"
    why="${why%%;*}"
    why="${why% }"
    if [[ "${#why}" -gt 21 ]]; then why="${why:0:18}..."; fi
    record "$label" "SKIP (${why})" "${dt}s"
    SKIPS=$((SKIPS + 1))
  else
    record "$label" "PASS" "${dt}s"
  fi
}

# gate_test: cargo test with auditable pass/fail totals summed from the
# libtest "test result:" summary lines across every suite + doctest.
gate_test() {
  GATE_NO=$((GATE_NO + 1))
  local label="cargo test --workspace --locked"
  local log="$LOGDIR/03-cargo-test.log" t0="$SECONDS"
  echo
  echo "== gate ${GATE_NO}: ${label} =="
  stream "$log" cargo test --workspace --locked
  local dt=$((SECONDS - t0))
  local tp tf ti
  read -r tp tf ti <<<"$(awk '
    /^test result:/ {
      for (i = 2; i <= NF; i++) {
        if ($i == "passed;")  p += $(i - 1)
        else if ($i == "failed;")   f += $(i - 1)
        else if ($i == "ignored;")  n += $(i - 1)
      }
    }
    END { print p + 0, f + 0, n + 0 }' "$log")"
  echo "cargo test totals: ${tp} passed / ${tf} failed / ${ti} ignored"
  if [[ "$RC" -eq 0 ]]; then
    record "$label" "PASS (${tp} passed / ${tf} failed)" "${dt}s"
  else
    record "$label" "FAIL (exit $RC; ${tp} passed / ${tf} failed)" "${dt}s"
    FAILS=$((FAILS + 1))
  fi
}

# gate_battery: gate 6, the domain e2e battery (skippable via --no-e2e).
gate_battery() {
  GATE_NO=$((GATE_NO + 1))
  local label="  domain e2e battery (total)"
  echo
  echo "== gate ${GATE_NO}: domain e2e battery =="
  if [[ "$RUN_E2E" -eq 0 ]]; then
    echo "SKIP domain e2e battery (--no-e2e)"
    record "$label" "SKIP (--no-e2e)" "0s"
    SKIPS=$((SKIPS + 1))
    return 0
  fi
  local t0="$SECONDS" s f0="$FAILS"
  for s in "${BATTERY[@]}"; do
    e2e_row "$s"
  done
  if [[ "$FAILS" -gt "$f0" ]]; then
    record "$label" "FAIL (see rows above)" "$((SECONDS - t0))s"
  else
    record "$label" "PASS (see rows above)" "$((SECONDS - t0))s"
  fi
}

summary() {
  local i
  echo
  echo "=========================== gate summary ==========================="
  printf '%-3s %-50s %-28s %s\n' "#" "gate" "status" "time"
  printf '%-3s %-50s %-28s %s\n' "---" "----" "----" "----"
  for i in "${!ROW_NAME[@]}"; do
    printf '%-3s %-50s %-28s %s\n' "$((i + 1))" "${ROW_NAME[$i]}" \
      "${ROW_STATUS[$i]}" "${ROW_TIME[$i]}"
  done
  echo "-------------------------------------------------------------------"
  echo "legend: PASS = gate green | FAIL = gate red (fails the run) |"
  echo "        SKIP = pass-with-skip (Q25: missing precondition is a"
  echo "        configuration state, not a conformance break) |"
  echo "        N/A (manual) = human checklist row, never fails the run."
  echo "totals: gates=${#ROW_NAME[@]} fails=${FAILS} skips=${SKIPS}"
}

# --- --list: print the gate list without running anything -------------------
if [[ "$LIST_ONLY" -eq 1 ]]; then
  e2e_note=""
  if [[ "$RUN_E2E" -eq 0 ]]; then e2e_note="  [skipped via --no-e2e]"; fi
  cat <<LIST
gate 1: cargo fmt --check (workspace members)
gate 2: cargo build --workspace --locked
gate 3: cargo test --workspace --locked   (prints pass/fail totals)
gate 4: cargo clippy --workspace --all-targets --locked -- -D warnings
gate 5: scripts/golden-conformance.sh
gate 6: domain e2e battery${e2e_note}:
LIST
  for s in "${BATTERY[@]}"; do
    extra=""
    if [[ " $ROOT_GATED " == *" $s "* ]]; then extra="  (root-gated here)"; fi
    echo "  - scripts/$s$extra"
  done
  echo "gate 7: scripts/cli-flag-parity-test.sh"
  echo "gate 8: SSOT lock-step row (index.md + plan/*.md + decisions.md) [manual]"
  exit 0
fi

# --- run --------------------------------------------------------------------
LOGDIR="$(mktemp -d /tmp/init-pro-all-gates.XXXXXX)"
KEEP="${KEEP_LOGS:-0}"
cleanup() {
  local rc=$?
  trap - EXIT
  if [[ "$FAILS" -gt 0 || "$KEEP" == "1" ]]; then
    echo "gate logs kept at: $LOGDIR"
  else
    rm -rf "$LOGDIR"
  fi
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

echo "init-pro all-gates (Sprint DoD runner)"
echo "gate log dir: $LOGDIR (kept on FAIL or KEEP_LOGS=1)"
if [[ "$RUN_E2E" -eq 0 ]]; then
  echo "note: --no-e2e given, gate 6 will be SKIPped"
fi

FMT_MEMBER_ARGS="$(cargo metadata --no-deps --format-version 1 \
  | python3 -c 'import json,sys; print(" ".join("-p "+m["name"] for m in json.load(sys.stdin)["packages"]))')"
gate "cargo fmt --check (workspace members)" "01-cargo-fmt" \
  cargo fmt --check $FMT_MEMBER_ARGS
gate "cargo build --workspace --locked" "02-cargo-build" cargo build --workspace --locked
gate_test
gate "cargo clippy --workspace --all-targets -D warnings" \
  "04-cargo-clippy" cargo clippy --workspace --all-targets --locked -- -D warnings
gate "golden-conformance (T0.6)" "05-golden-conformance" "$SCRIPT_DIR/golden-conformance.sh"
gate_battery
gate "cli-flag-parity (T0.4)" "07-cli-flag-parity" "$SCRIPT_DIR/cli-flag-parity-test.sh"

# Gate 8: SSOT lock-step (plans/init-pro/index.md + plan/*.md + decisions.md
# + CHANGELOG.md must move together). Not automatable: a MANUAL row.
GATE_NO=$((GATE_NO + 1))
echo
echo "== gate ${GATE_NO}: SSOT lock-step (manual) =="
echo "MANUAL: verify index.md + plan/*.md + decisions.md moved in lock-step."
record "SSOT lock-step (index/plan/decisions)" "N/A (manual)" "-"

summary
if [[ "$FAILS" -gt 0 ]]; then
  echo "RESULT: FAIL (${FAILS} gate(s) failed; logs above)"
  exit 1
fi
echo "RESULT: PASS (no FAIL rows; SKIPs are pass-with-skip per Q25)"
