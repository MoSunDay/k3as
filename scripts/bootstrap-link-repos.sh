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
# Optional per-repo `also_link` (TOML string array, ';'-joined into one TSV
# column) names EXTRA paths that must exist as RELATIVE symlinks to the
# repo's MAIN target. Used when an upstream workspace references a pin
# through its OWN internal relative path (any-home's Cargo.toml resolves
# nfsserve at link_repos/nfsserve relative to the any-home root): without
# the symlink that path would either not resolve or hold a SECOND,
# divergent copy of the repo. Bootstrap creates missing links, verifies
# existing ones resolve to the main target, and migrates a legacy REAL
# directory at an also_link path to the symlink — only when it is a git
# repo at the pinned sha whose only dirt is the patch marker (link_repos
# trees are reproducible artifacts, so replacing them is safe); dirty or
# non-git trees die (--force downgrades to a warning, leaving them alone).
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
# Parse link_repos.lock into TSV records: name,url,sha,path,patch,marker,also.
# Empty patch/marker fields are emitted as "-" (IFS=$'\t' read drops empty
# fields); path defaults to link_repos/<name>. `also_link` (TOML string
# array) is flattened into ONE column with ';' separators. mawk-compatible
# (no gawk match() array capture).
# ---------------------------------------------------------------------------
parse_lock() {
  awk '
    function flush() {
      if (name == "") return
      if (url == "" || sha == "") { print "BAD-RECORD" > "/dev/stderr"; exit 2 }
      if (path == "") path = "link_repos/" name
      if (patch == "") patch = "-"
      if (marker == "") marker = "-"
      if (also == "") also = "-"
      printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n", name, url, sha, path, patch, marker, also
      name = ""; url = ""; sha = ""; path = ""; patch = ""; marker = ""; also = ""
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
      else if (k == "also_link") {
        # TOML string array: ["a", "b"] -> "a;b" (empty -> "").
        sub(/^\[/, "", v); sub(/\]$/, "", v)
        also = ""
        n = split(v, arr, ",")
        for (i = 1; i <= n; i++) {
          t = arr[i]
          sub(/^[ \t]+/, "", t); sub(/[ \t]+$/, "", t)
          sub(/^"/, "", t); sub(/"$/, "", t)
          if (t == "") continue
          also = (also == "" ? t : also ";" t)
        }
      }
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

# Relative path from directory $1 to path $2 (both absolute, normalized,
# no trailing slash). Component-boundary aware ("a/bc" never matches
# "a/b/..."); when no prefix is shared, falls back to the absolute $2.
relpath() { # $1 from-dir, $2 to-path
  local from="${1%/}" to="${2%/}" up=""
  local common="$from"
  while [[ "$to" != "$common" && "$to" != "$common/"* ]]; do
    common="$(dirname "$common")"
    up="../$up"
    [[ "$common" == "/" ]] && break
  done
  if   [[ "$to" == "$common" ]];   then printf '%s\n' "${up%/}"
  elif [[ "$to" == "$common/"* ]]; then printf '%s\n' "$up${to#"$common"/}"
  else                                  printf '%s\n' "$to"
  fi
}

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

# Extra alias paths (also_link) that must be RELATIVE symlinks to the main
# target, so an upstream workspace's internal relative path deps resolve to
# the SAME repo instead of a second copy (see lock header).
ensure_also_links() { # $1 name, $2 main target (abs), $3 sha, $4 marker ('-' none), $5 also (';'-joined, '-' none)
  local name="$1" main="$2" sha="$3" marker="$4" also="$5"
  [[ "$also" == "-" || -z "$also" ]] && return 0
  local resolved_main; resolved_main="$(readlink -f "$main")"
  local link abs rel
  while IFS= read -r link; do
    [[ -z "$link" ]] && continue
    [[ "$link" != /* ]] || die "$name: also_link entry must be repo-root relative, got '$link'"
    abs="$ROOT/$link"
    if [[ "$abs" == "$main" ]]; then continue; fi
    rel="$(relpath "$(dirname "$abs")" "$main")"
    if [[ -L "$abs" ]]; then
      local resolved; resolved="$(readlink -f "$abs")"
      if [[ "$resolved" == "$resolved_main" ]]; then
        log "$name: also_link $link -> $(readlink "$abs") verified"
      else
        fail_or_warn "$name: also_link $link resolves to $resolved, expected $resolved_main"
      fi
    elif [[ -d "$abs" ]]; then
      # Legacy nested layout: migrate the real dir to a symlink, but only
      # when it provably reproduces from the pin (git repo at the pinned
      # sha whose only dirt is the patch marker) — link_repos trees are
      # materialized artifacts, safe to replace under that guarantee.
      local lhead; lhead="$(repo_head "$abs")"
      if [[ ! -d "$abs/.git" ]]; then
        fail_or_warn "$name: also_link $link is a real non-git directory; refusing to replace"
      elif [[ "$lhead" != "$sha" ]]; then
        fail_or_warn "$name: also_link $link is a git repo at $lhead (pin $sha); refusing to replace"
      elif repo_dirty "$abs" && [[ "$marker" == "-" || ! -e "$abs/$marker" ]]; then
        fail_or_warn "$name: also_link $link is dirty beyond the patch marker; refusing to replace"
      else
        rm -rf "$abs"
        ln -s "$rel" "$abs"
        log "$name: migrated legacy layout: $link real dir -> symlink $rel"
      fi
    elif [[ -e "$abs" ]]; then
      fail_or_warn "$name: also_link $link exists and is not a directory"
    else
      mkdir -p "$(dirname "$abs")"
      ln -s "$rel" "$abs"
      log "$name: linked $link -> $rel"
    fi
  done < <(tr ';' '\n' <<< "$also")
}

ensure_repo() { # $1 name, $2 url, $3 sha, $4 path, $5 patch ('-' none), $6 marker, $7 also ('-' none)
  local name="$1" url="$2" sha="$3" target="$ROOT/$4" patch="$5" marker="$6" also="$7"
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
    # HTTP/1.1: some egress proxies mangle HTTP/2 git transfers (GnuTLS
    # recv error -110); 1.1 is universally compatible. lowSpeed aborts
    # stalled transfers (<1 KiB/s for 60s) so the retry loop engages
    # instead of hanging forever.
    local attempt
    for attempt in 1 2 3; do
      if git -c http.version=HTTP/1.1 \
              -c http.lowSpeedLimit=1024 -c http.lowSpeedTimeout=60 \
              clone --quiet "$url" "$target"; then
        break
      fi
      rm -rf "$target"
      [[ $attempt -eq 3 ]] && die "$name: clone failed after 3 attempts ($url)"
      warn "$name: clone attempt $attempt failed, retrying"
    done
    git -C "$target" checkout --quiet --detach "$sha" \
      || die "$name: pinned sha $sha not reachable in $url"
    log "$name: materialized at $sha"
  fi

  if [[ "$patch" != "-" ]]; then apply_patch "$target" "$patch" "$marker"; fi

  local head; head="$(repo_head "$target")"
  [[ "$head" == "$sha" ]] || die "$name: final HEAD $head != pinned $sha"

  ensure_also_links "$name" "$target" "$sha" "$marker" "$also"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
mapfile -t RECORDS < <(parse_lock)
[[ ${#RECORDS[@]} -gt 0 ]] || die "no [[repo]] records parsed from $LOCK"

for rec in "${RECORDS[@]}"; do
  IFS=$'\t' read -r name url sha path patch marker also <<< "$rec"
  ensure_repo "$name" "$url" "$sha" "$path" "$patch" "$marker" "$also"
done

log "all ${#RECORDS[@]} pinned repos verified. build with: cargo build --workspace --locked"
