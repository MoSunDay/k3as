#!/usr/bin/env bash
# Q48 / T4.11 safety boundary: `unsafe` lives in `crates/nsrun` and nowhere
# else. Every other workspace crate keeps `#![forbid(unsafe_code)]`, and
# every unsafe item in nsrun carries a `SAFETY:` note (the compiler-side
# half is clippy::undocumented_unsafe_blocks, denied in nsrun/lib.rs).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
NSRUN="$ROOT/crates/nsrun"

fail=0
ok() { echo "OK   $*"; }
bad() { echo "FAIL $*"; fail=1; }

# (1) unsafe tokens outside nsrun. Comments are stripped so doc prose such
#     as "no unsafe" does not trip the audit.
hits="$(python3 - "$ROOT" <<'PY'
import io, re, sys
from pathlib import Path
root = Path(sys.argv[1])
token = re.compile(r"\bunsafe\b")
out = []
for path in sorted((root / "crates").rglob("*.rs")):
    if "nsrun" in path.parts:
        continue
    for n, line in enumerate(path.read_text(errors="replace").splitlines(), 1):
        code = line.split("//", 1)[0]
        if token.search(code):
            out.append(f"{path.relative_to(root)}:{n}: {line.strip()}")
print("\n".join(out))
PY
)"
if [[ -z "$hits" ]]; then
  ok "(1) no unsafe outside crates/nsrun"
else
  bad "(1) unsafe found outside crates/nsrun:"
  echo "$hits" | sed 's/^/      /'
fi

# (2) crate roots outside nsrun forbid unsafe.
roots=()
for lib in "$ROOT"/crates/*/src/lib.rs; do roots+=("$lib"); done
roots+=("$ROOT/crates/init-pro/src/main.rs") # bin-only crate
for root in "${roots[@]}"; do
  [[ "$root" == "$NSRUN"/* ]] && continue
  if grep -q '#!\[forbid(unsafe_code)\]' "$root"; then
    ok "(2) $(realpath --relative-to="$ROOT" "$root") forbids unsafe"
  else
    bad "(2) missing #![forbid(unsafe_code)] in ${root#"$ROOT"/}"
  fi
done

# (3) nsrun unsafe items are documented.
undoc="$(python3 - "$NSRUN" <<'PY'
import re, sys
from pathlib import Path
root = Path(sys.argv[1])
item = re.compile(r"(^|\s)unsafe(\s*\{|\s+fn|\s+impl|\s+trait)")
out = []
for path in sorted(root.rglob("*.rs")):
    lines = path.read_text().splitlines()
    for i, line in enumerate(lines):
        if not item.search(line.split("//", 1)[0]):
            continue
        window = "\n".join(lines[max(0, i - 8):i + 1])
        if "SAFETY:" not in window and "# Safety" not in window:
            out.append(f"{path.relative_to(root)}:{i + 1}: {line.strip()}")
print("\n".join(out))
PY
)"
if [[ -z "$undoc" ]]; then
  ok "(3) every nsrun unsafe item has a SAFETY note"
else
  bad "(3) nsrun unsafe items without a SAFETY note (the item + the 8 lines above it):"
  echo "$undoc" | sed 's/^/      /'
fi

# (4) the nsrun lint gates stay armed.
for lint in 'deny(unsafe_op_in_unsafe_fn)' 'deny(clippy::undocumented_unsafe_blocks)'; do
  if grep -qF "#![$lint]" "$NSRUN/src/lib.rs"; then
    ok "(4) nsrun declares #![$lint]"
  else
    bad "(4) nsrun/src/lib.rs lost #![$lint]"
  fi
done

echo "----"
if [[ "$fail" -eq 0 ]]; then
  echo "unsafe-audit: PASS (Q48 boundary intact)"
else
  echo "unsafe-audit: FAIL"
fi
exit "$fail"
