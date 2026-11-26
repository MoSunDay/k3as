#!/usr/bin/env bash
# e2e case registry gate (batch E, 长稳清扫 2026-11-26): the machine check
# behind e2e/cases.toml + e2e/cases.d/*.toml. It is the gate that keeps the
# e2e inventory honest in three directions:
#
#   1. registry <-> runner parity: every BATTERY entry of scripts/all-gates.sh
#      is a `runner = "battery"` suite and vice versa; every golden shard and
#      every scripts/*-e2e.sh on disk is registered (no orphan suites);
#   2. registry <-> SSOT coverage: every `done` TODO in plans/init-pro/index.md
#      maps to a covering suite (battery/golden/gate) or carries an explicit
#      exemption with a reason in e2e/cases.toml (weak-only coverage -- ci,
#      manual, unwired -- is reported, never fatal);
#   3. registry <-> evidence: with `--logs <dir>` (the all-gates log dir) every
#      declared case id must appear as an OK/SKIP token in the suite's gate
#      log, so a renamed/removed case cannot linger in the registry.
#
# SKIP doctrine (Q25): a suite whose log leads with SKIP (precondition missing)
# is counted as "skipped", never as a missing-case failure; suites that FAILED
# in the battery (E2E_FAILED_SCRIPTS, set by all-gates.sh) are left to the
# battery row -- this gate never double-reports a failure.
#
# Usage: scripts/e2e-coverage.sh [--logs <gate-log-dir>] [--list]
#   --logs <dir>  also verify declared case ids against the gate logs
#   --list        print the registry grouped by layer, then exit 0
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INDEX="$ROOT/plans/init-pro/index.md"
GATES="$ROOT/scripts/all-gates.sh"
MANIFEST="$ROOT/e2e/cases.toml"
LOGS=""
LIST=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --logs) LOGS="${2:-}"; shift 2 ;;
    --list) LIST=1; shift ;;
    -h|--help) sed -n 's/^# Usage:/Usage:/p; s/^#   --/--/p' "$0"; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; exit 2 ;;
  esac
done

PASS=0; FAIL=0
ok()  { echo "OK   $*"; PASS=$((PASS + 1)); }
bad() { echo "FAIL $*" >&2; FAIL=$((FAIL + 1)); }

# --- registry state (parallel arrays; one slot per [[suite]]) --------------
SUITE_N=0; cur=-1
S_ID=(); S_SCRIPT=(); S_RUNNER=(); S_LAYER=(); S_TODOS=(); S_CASES=(); S_SKIP=(); S_TITLE=(); S_FILE=()
EXEMPT_KEYS=(); EXEMPT_WHY=()
COVERING=""; WEAK=""

trim() { local s="$1"; s="${s#"${s%%[![:space:]]*}"}"; s="${s%"${s##*[![:space:]]}"}"; printf '%s' "$s"; }
unquote() { local v="$1"; v="${v#\"}"; v="${v%\"}"; printf '%s' "$v"; }
array_tokens() {  # [ "a", "b" ] -> "a b"
  local v="$1"; v="${v#[}"; v="${v%]}"; v="${v//\"/}"; v="${v//,/ }"; trim "$v"
}

# load_file <toml>: line-oriented TOML subset (strings, arrays, [[suite]]).
KEY_RE='^([A-Za-z0-9_".]+)[[:space:]]*=[[:space:]]*(.*)$'   # bare `key = value` line
load_file() {
  local f="$1" line key val section="" n=0   # `cur` is global: the suite slot
  while IFS= read -r line || [[ -n "$line" ]]; do
    n=$((n + 1))
    line="${line%$'\r'}"
    line="$(trim "$line")"
    [[ -z "$line" || "$line" == \#* ]] && continue
    if [[ "$line" == "[[suite]]" ]]; then
      cur=$SUITE_N; SUITE_N=$((SUITE_N + 1)); section="suite"
      S_ID+=(""); S_SCRIPT+=(""); S_RUNNER+=(""); S_LAYER+=(""); S_TODOS+=("")
      S_CASES+=(""); S_SKIP+=(""); S_TITLE+=(""); S_FILE+=("$f")
      continue
    fi
    if [[ "$line" =~ ^\[([a-z_]+)\]$ ]]; then section="${BASH_REMATCH[1]}"; continue; fi
    [[ "$line" =~ $KEY_RE ]] || { bad "$(basename "$f"):$n: unparsable line: $line"; continue; }
    key="${BASH_REMATCH[1]}"; val="${BASH_REMATCH[2]}"
    val="$(trim "${val%%[[:space:]]#*}")"   # trailing comments (no '#' inside values)
    case "$section" in
      meta) ;;                       # [meta] is documentation for humans
      coverage_exemptions)
        [[ -n "$val" ]] || { bad "$(basename "$f"):$n: empty exemption"; continue; }
        EXEMPT_KEYS+=("$(unquote "$key")"); EXEMPT_WHY+=("$(unquote "$val")") ;;
      suite)
        [[ $cur -ge 0 ]] || { bad "$(basename "$f"):$n: key outside [[suite]]: $key"; continue; }
        case "$key" in
          id)     S_ID[$cur]="$(unquote "$val")" ;;
          script) S_SCRIPT[$cur]="$(unquote "$val")" ;;
          runner) S_RUNNER[$cur]="$(unquote "$val")" ;;
          layer)  S_LAYER[$cur]="$(unquote "$val")" ;;
          todos)  S_TODOS[$cur]="$(array_tokens "$val")" ;;
          cases)  S_CASES[$cur]="$(array_tokens "$val")" ;;
          skip)   S_SKIP[$cur]="$(array_tokens "$val")" ;;
          title)  S_TITLE[$cur]="$(unquote "$val")" ;;
          *) bad "$(basename "$f"):$n: unknown suite key '$key'" ;;
        esac ;;
      *) bad "$(basename "$f"):$n: unknown section [$section]" ;;
    esac
  done <"$f"
}

# --- load registry ---------------------------------------------------------
shopt -s nullglob
FILES=("$MANIFEST" "$ROOT"/e2e/cases.d/*.toml)
shopt -u nullglob
for f in "${FILES[@]}"; do [[ -f "$f" ]] || { bad "manifest missing: $f"; continue; }; load_file "$f"; done
if [[ -f "$MANIFEST" ]]; then
  COVERING="$(awk '/^covering_runners/{sub(/^[^=]*=[[:space:]]*\[/,""); sub(/\].*/,""); gsub(/[",]/,""); print}' "$MANIFEST")"
  WEAK="$(awk '/^weak_runners/{sub(/^[^=]*=[[:space:]]*\[/,""); sub(/\].*/,""); gsub(/[",]/,""); print}' "$MANIFEST")"
fi
[[ -n "$COVERING" ]] || COVERING="battery golden gate"
[[ -n "$WEAK" ]] || WEAK="ci manual unwired"

# --- index.md status table -------------------------------------------------
TODO_IDS=(); TODO_STATUS=(); TODO_LAYER=()
while IFS=' ' read -r id layer st; do
  [[ -n "$id" ]] || continue
  TODO_IDS+=("$id"); TODO_LAYER+=("$layer"); TODO_STATUS+=("$st")
done < <(awk '/^\|[[:space:]]*T[0-9]+\.[0-9]+b?[[:space:]]*\|/ {
           gsub(/\\\|/, "\001")                       # mask escaped pipes inside cells
           split($0, c, "|"); id = c[2]; layer = c[3]; s = c[5]
           gsub(/ /, "", id); gsub(/ /, "", layer); gsub(/^ +| +$/, "", s); split(s, a, " ")
           print id, layer, a[1] }' "$INDEX")
[[ ${#TODO_IDS[@]} -gt 0 ]] || { echo "FAIL no TODO rows parsed from $INDEX" >&2; exit 1; }
is_todo() { local t; for t in "${TODO_IDS[@]}"; do [[ "$t" == "$1" ]] && return 0; done; return 1; }
status_of() { local i; for i in "${!TODO_IDS[@]}"; do [[ "${TODO_IDS[$i]}" == "$1" ]] && { printf '%s' "${TODO_STATUS[$i]}"; return; }; done; }
is_exempt() { local k; for k in "${EXEMPT_KEYS[@]}"; do [[ "$k" == "$1" ]] && return 0; done; return 1; }

# --- --list: registry grouped by layer --------------------------------------
if [[ "$LIST" -eq 1 ]]; then
  printf '%-3s %-9s %-42s %-16s %s\n' "L" "runner" "script" "todos" "cases"
  for layer in 0 1 2 3 4 5 6 7 8 9; do
    for i in "${!S_ID[@]}"; do
      [[ "${S_LAYER[$i]}" == "$layer" ]] || continue
      printf '%-3s %-9s %-42s %-16s %s\n' "$layer" "${S_RUNNER[$i]}" "${S_SCRIPT[$i]}" "${S_TODOS[$i]}" "${S_CASES[$i]}"
    done
  done
  exit 0
fi

# --- 1. registry <-> runner parity ------------------------------------------
declare -A SEEN_ID=() SEEN_SCRIPT=()
BATTERY=()
if [[ -f "$GATES" ]]; then
  BATTERY=($(awk '/^BATTERY=\(/{f=1; next} f&&/^\)/{exit} f{gsub(/[[:space:]]/,""); if ($0 ~ /\.sh$/) print}' "$GATES"))
else
  bad "gate runner not found: $GATES"
fi
REG_BATTERY=()
for i in "${!S_ID[@]}"; do
  id="${S_ID[$i]}"; sc="${S_SCRIPT[$i]}"; rn="${S_RUNNER[$i]}"
  [[ -n "$id" && -n "$sc" && -n "$rn" ]] || { bad "suite #$i incomplete (id/script/runner)"; continue; }
  [[ -z "${SEEN_ID[$id]:-}" ]] || bad "duplicate suite id: $id"; SEEN_ID[$id]=1
  [[ -z "${SEEN_SCRIPT[$sc]:-}" ]] || bad "duplicate script registration: $sc ($id)"; SEEN_SCRIPT[$sc]=1
  [[ -f "$ROOT/$sc" ]] || bad "$id: script missing: $sc"
  case "$rn" in battery|golden|gate|ci|manual|unwired|tool) ;; *) bad "$id: unknown runner '$rn'" ;; esac
  [[ "${S_LAYER[$i]}" =~ ^[0-9]$ ]] || bad "$id: layer must be 0..9 (got '${S_LAYER[$i]}')"
  [[ "$rn" == "battery" ]] && REG_BATTERY+=("$(basename "$sc")")
  for t in ${S_TODOS[$i]}; do is_todo "$t" || bad "$id: unknown TODO id '$t' (not in index.md)"; done
  declare -A cs=(); for c in ${S_CASES[$i]}; do [[ -z "${cs[$c]:-}" ]] || bad "$id: duplicate case id '$c'"; cs[$c]=1; done; unset cs
  [[ "$rn" != "tool" && "$rn" != "unwired" ]] || [[ -n "${S_TITLE[$i]}" ]] || bad "$id: $rn suites need a title justifying themselves"
done
[[ $SUITE_N -ge 40 ]] && ok "registry loaded: $SUITE_N suites from $((${#FILES[@]})) files" || bad "registry too small ($SUITE_N suites) -- parsing regression?"

bat_diff() { diff <(printf '%s\n' "${BATTERY[@]}" | sort) <(printf '%s\n' "${REG_BATTERY[@]}" | sort) || true; }
if [[ ${#BATTERY[@]} -gt 0 ]]; then
  d="$(bat_diff)"
  if [[ -z "$d" ]]; then ok "BATTERY parity: ${#BATTERY[@]} scripts == runner=battery suites"; else bad "BATTERY <-> registry drift:"$'\n'"$d"; fi
else
  bad "no BATTERY array parsed from $GATES"
fi
# every e2e script on disk is registered (any runner); golden shards too
shopt -s nullglob
for sc in "$ROOT"/scripts/*-e2e.sh "$ROOT"/scripts/golden/suites/*.sh; do
  rel="${sc#"$ROOT"/}"
  [[ -n "${SEEN_SCRIPT[$rel]:-}" ]] || bad "unregistered script on disk: $rel"
done
shopt -u nullglob
for g in unsafe-audit.sh engine-singlethread-check.sh cli-flag-parity-test.sh golden-conformance.sh; do
  [[ -n "${SEEN_SCRIPT[scripts/$g]:-}" ]] || bad "static gate not registered: $g"
done
ok "on-disk parity: $(ls "$ROOT"/scripts/*-e2e.sh | wc -l) e2e scripts + $(ls "$ROOT"/scripts/golden/suites/*.sh | wc -l) golden shards all registered"

# --- 2. registry <-> SSOT coverage ------------------------------------------
COVERED=0; WEAKLY=0; EXEMPTED=0; PARTIAL=()
for i in "${!TODO_IDS[@]}"; do
  t="${TODO_IDS[$i]}"; st="${TODO_STATUS[$i]}"; cov=0; weak=0; suites=""
  for s in "${!S_ID[@]}"; do
    [[ " ${S_TODOS[$s]} " == *" $t "* ]] || continue
    suites="$suites ${S_ID[$s]}"
    rn="${S_RUNNER[$s]}"
    if [[ " $COVERING " == *" $rn "* ]]; then cov=1; elif [[ " $WEAK " == *" $rn "* ]]; then weak=1; fi
  done
  case "$st" in
    done*)
      if [[ $cov -eq 1 ]]; then COVERED=$((COVERED + 1))
      elif [[ $weak -eq 1 ]]; then WEAKLY=$((WEAKLY + 1)); echo "weak $t: only non-gated suites cover it ($suites)"
      elif is_exempt "$t"; then EXEMPTED=$((EXEMPTED + 1))
      else bad "done TODO $t has no e2e suite and no exemption in e2e/cases.toml"; fi ;;
    in-progress*) [[ -n "$suites" ]] && PARTIAL+=("$t") ;;
  esac
done
DONE_N=0; for st in "${TODO_STATUS[@]}"; do [[ "$st" == done* ]] && DONE_N=$((DONE_N + 1)); done
ok "coverage: $DONE_N done TODOs -> $COVERED covered, $WEAKLY weak-only, $EXEMPTED exempt, $((DONE_N - COVERED - WEAKLY - EXEMPTED)) uncovered"
echo "note: in-progress TODOs with partial suites: ${PARTIAL[*]:-none}"
for k in "${EXEMPT_KEYS[@]}"; do is_todo "$k" || bad "exemption for unknown TODO: $k"; [[ "$(status_of "$k")" == done* ]] || bad "exemption for a TODO that is not done (drop it or keep coverage honest): $k"; done

# --- 3. registry <-> evidence (gate logs) -----------------------------------
seen_tokens() {  # one token per line: case ids + expanded A-B / A/B ranges
  local log="$1"
  { grep -E '^(OK|PASS|SKIP)[[:space:]]' "$log" 2>/dev/null || true; } \
    | { grep -oE '(^|[^A-Za-z0-9])[A-Z]+[0-9]+[a-z]?(-[A-Z]*[0-9]+)?' || true; } \
    | sed -E 's/^[^A-Z]*//' \
    | while IFS= read -r tok; do
        if [[ "$tok" =~ ^([A-Z]+)([0-9]+)[a-z]?-([A-Z]*)([0-9]+)$ ]]; then
          p="${BASH_REMATCH[1]}"; a="${BASH_REMATCH[2]}"; p2="${BASH_REMATCH[3]}"; b="${BASH_REMATCH[4]}"
          if [[ -z "$p2" || "$p2" == "$p" ]] && (( a <= b )); then
            w=${#a}; for (( n = a; n <= b; n++ )); do printf '%s%0*d\n' "$p" "$w" "$n"; done
          else echo "$tok"; fi
        else echo "$tok"; fi
      done | sort -u
}
if [[ -n "$LOGS" ]]; then
  VERIFIED=0; SKIPPED=0
  GOLDEN_LOG="$(ls "$LOGS"/*golden-conformance.log 2>/dev/null | head -1 || true)"
  for i in "${!S_ID[@]}"; do
    rn="${S_RUNNER[$i]}"; [[ "$rn" == "battery" || "$rn" == "golden" ]] || continue
    base="$(basename "${S_SCRIPT[$i]}")"
    case "$rn" in battery) log="$LOGS/e2e-$base.log" ;; golden) log="${GOLDEN_LOG:-}" ;; esac
    [[ -n "$log" && -f "$log" ]] || { bad "${S_ID[$i]}: no gate log ($log)"; continue; }
    [[ " ${E2E_FAILED_SCRIPTS:-} " != *" $base "* ]] || { echo "note: ${S_ID[$i]} FAILED in the battery; log check left to that row"; continue; }
    first="$(awk 'NF { print; exit }' "$log")"
    if [[ "$first" == SKIP* ]]; then SKIPPED=$((SKIPPED + 1)); continue; fi
    [[ -n "${S_CASES[$i]}" ]] || continue
    seen="$(seen_tokens "$log" || true)"   # total: a log with no tokens is a miss, not a crash
    for c in ${S_CASES[$i]}; do
      if [[ "$c" =~ [a-z] ]]; then
        grep -qE "^(OK|PASS|SKIP)[[:space:]]+$c([^A-Za-z0-9_-]|$)" "$log" || bad "${S_ID[$i]}: case $c not seen in $(basename "$log")"
      else
        grep -qx -- "$c" <<<"$seen" || bad "${S_ID[$i]}: case $c not seen in $(basename "$log")"
      fi
    done
    VERIFIED=$((VERIFIED + 1))
  done
  ok "evidence: $VERIFIED suite logs verified against declared case ids ($SKIPPED skipped suites)"
fi

TOTAL_CASES=0; for i in "${!S_ID[@]}"; do TOTAL_CASES=$((TOTAL_CASES + $(echo "${S_CASES[$i]}" | wc -w))); done
echo
echo "e2e-coverage: $SUITE_N suites / $TOTAL_CASES cases / layers L0-L9 -- done TODOs covered=$COVERED weak=$WEAKLY exempt=$EXEMPTED; checks failed=$FAIL"
if [[ "$FAIL" -gt 0 ]]; then echo "e2e-coverage: FAIL"; exit 1; fi
echo "e2e-coverage: PASS"
