#!/usr/bin/env bash
# T0.2b / Q38: reproducibly materialize the pinned link_repos path deps.
#
# Consumes link_repos.lock (TOML manifest, repo-root relative). Semantics:
#
#   missing target   -> git clone <url> + git checkout --detach <sha>
#   real git dir     -> verify HEAD == <sha>; auto-checkout when clean,
#                       refuse when dirty (unless --force); fetch first so
#                       shallow/partial checkouts can still reach the pin
#   symlink target   -> LOCAL DEV layout (link_repos/<name> -> /root/<name>):
#                       verify the resolved repo HEAD == <sha>, warn on dirty
#                       (refuse unless --force; a patch_marker exempts the
#                       expected patched dirt), NEVER modify anything.
#
# Optional per-repo `patch` is applied after checkout (idempotent via
# patch_marker; hard-fails on upstream drift so silent breakage cannot pass).
#
# Idempotent: a second run only verifies. Any failure exits non-zero.
# `--force` downgrades verify failures to warnings (escape hatch for live
# WIP trees); it never discards or resets anything.
#
# The real dirs link_repos/{argo-workflows,flannel} are reference-only
# (non-build deps) and intentionally not managed here.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOCK="$ROOT/link_repos.lock"

# Public https pins only; never prompt for credentials.
export GIT_TERMINAL_PROMPT=0

FORCE=0
if [[ "${1:-}" == "--force" ]]; then FORCE=1; fi

log()  { echo "[bootstrap] $*"; }
warn() { echo "[bootstrap] WARN: $*" >&2; }
die()  { echo "[bootstrap] ERROR: $*" >&2; exit 1; }

[[ -f "$LOCK" ]] || die "lock manifest not found: $LOCK"

# ---------------------------------------------------------------------------
# Parse link_repos.lock into TSV records: name,url,sha,path,patch,marker.
# Empty patch/marker fields are emitted as "-" (IFS=$'\t' read drops empty
# fields); path defaults to link_repos/<name>. mawk-compatible (no gawk
# match() array capture).
# ---------------------------------------------------------------------------
parse_lock() {
  awk '
    function flush() {
      if (name == "") return
      if (url == "" || sha == "") { print "BAD-RECORD" > "/dev/stderr"; exit 2 }
      if (path == "") path = "link_repos/" name
      if (patch == "") patch = "-"
      if (marker == "") marker = "-"
      printf "%s\t%s\t%s\t%s\t%s\t%s\n", name, url, sha, path, patch, marker
      name = ""; url = ""; sha = ""; path = ""; patch = ""; marker = ""
    }
    /^\[\[repo\]\]/ { flush(); next }
    {
      line = $0
      sub(/^[ \t]+/, "", line); sub(/[ \t]+$/, "", line)
      if (line == "" || line ~ /^#/) next
      eq = index(line, "="); if (eq == 0) next
      k = substr(line, 1, eq - 1); v = substr(line, eq + 1)
      sub(/^[ \t]+/, "", k); sub(/[ \t]+$/, "", k)
      sub(/^[ \t]+/, "", v); sub(/[ \t]+$/, "", v)
      sub(/^"/, "", v); sub(/"$/, "", v)
      if      (k == "name")          name = v
      else if (k == "url")           url = v
      else if (k == "sha")           sha = v
      else if (k == "path")          path = v
      else if (k == "patch")         patch = v
      else if (k == "patch_marker")  marker = v
    }
    END { flush() }
  ' "$LOCK"
}

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
repo_head() { git -C "$1" rev-parse HEAD 2>/dev/null || echo ""; }

# Tracked modifications only (-uno): untracked artifacts (target/,
# node_modules/) do not signal dependency drift.
repo_dirty() { [[ -n "$(git -C "$1" status --porcelain -uno 2>/dev/null)" ]]; }

fail_or_warn() { # $1 message
  if [[ $FORCE -eq 1 ]]; then warn "$1 (continuing: --force)"; else die "$1"; fi
}

apply_patch() { # $1 repo dir, $2 patch (repo-root relative), $3 marker (repo-relative)
  if [[ -e "$1/$3" ]]; then
    log "$(basename "$1"): patch already applied (marker $3 present)"
    return 0
  fi
  if git -C "$1" apply --check "$ROOT/$2" 2>/dev/null; then
    git -C "$1" apply "$ROOT/$2"
    log "$(basename "$1"): applied $(basename "$2")"
  else
    fail_or_warn "$(basename "$1"): patch $2 did not apply cleanly (upstream drift?)"
  fi
}

ensure_repo() { # $1 name, $2 url, $3 sha, $4 path, $5 patch ('-' none), $6 marker
  local name="$1" url="$2" sha="$3" target="$ROOT/$4" patch="$5" marker="$6"
  [[ "$sha" =~ ^[0-9a-f]{40}$ ]] || die "$name: malformed sha '$sha' in lock"

  if [[ -L "$target" ]]; then
    # Local dev checkout: verify only, never modify.
    local resolved
    resolved="$(readlink -f "$target")"
    [[ -d "$resolved/.git" ]] || die "$name: symlink $target -> $resolved is not a git repo"
    local head; head="$(repo_head "$resolved")"
    if [[ "$head" != "$sha" ]]; then
      fail_or_warn "$name: symlinked dev checkout HEAD $head != pinned $sha"
    fi
    if repo_dirty "$resolved" && [[ ! -e "$resolved/$marker" || "$marker" == "-" ]]; then
      fail_or_warn "$name: symlinked dev checkout is dirty (WIP tree?)"
    fi
    log "$name: symlink dev checkout verified at $sha ($resolved)"
    return 0
  fi

  if [[ -e "$target" ]]; then
    [[ -d "$target/.git" ]] || die "$name: $target exists but is not a git repo"
    local head; head="$(repo_head "$target")"
    if [[ "$head" != "$sha" ]]; then
      if repo_dirty "$target"; then
        fail_or_warn "$name: HEAD $head != pinned $sha and tree is dirty"
      else
        # Refetch (covers shallow/partial clones) then move to the pin.
        git -C "$target" fetch --all --quiet \
          || fail_or_warn "$name: fetch failed while re-pinning"
        git -C "$target" checkout --quiet --detach "$sha" \
          || die "$name: pinned sha $sha not present after fetch"
        log "$name: re-pinned $head -> $sha"
      fi
    fi
    if repo_dirty "$target" && [[ "$marker" == "-" || ! -e "$target/$marker" ]]; then
      fail_or_warn "$name: materialized tree is dirty (expected only patched dirt)"
    fi
  else
    mkdir -p "$(dirname "$target")"
    log "$name: cloning $url"
    git clone --quiet "$url" "$target" || die "$name: clone failed ($url)"
    git -C "$target" checkout --quiet --detach "$sha" \
      || die "$name: pinned sha $sha not reachable in $url"
    log "$name: materialized at $sha"
  fi

  if [[ "$patch" != "-" ]]; then apply_patch "$target" "$patch" "$marker"; fi

  local head; head="$(repo_head "$target")"
  [[ "$head" == "$sha" ]] || die "$name: final HEAD $head != pinned $sha"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
mapfile -t RECORDS < <(parse_lock)
[[ ${#RECORDS[@]} -gt 0 ]] || die "no [[repo]] records parsed from $LOCK"

for rec in "${RECORDS[@]}"; do
  IFS=$'\t' read -r name url sha path patch marker <<< "$rec"
  ensure_repo "$name" "$url" "$sha" "$path" "$patch" "$marker"
done

log "all ${#RECORDS[@]} pinned repos verified. build with: cargo build --workspace --locked"
