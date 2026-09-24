#!/usr/bin/env bash
#
# G7 (spec 17): keep specs/ and src/ in step.
#
#   (a) every spec file is listed in the specs/README.md index table
#       (README.md itself and validation-*.md records are exempt);
#   (b) every top-level src/ module declares which spec it implements, in its
#       module doc comment. This is what makes "new module -> spec first" a
#       machine-checked rule instead of a convention nobody verifies.
#
# Usage: scripts/verify-spec-index.sh
# Exit codes: 0 = consistent, 1 = drift, 2 = usage.
set -euo pipefail

cd "$(dirname "$0")/.."

spec_lines=12
status=0

# --- (a) spec files are indexed -------------------------------------------
for spec in specs/*.md; do
  [ -f "$spec" ] || continue
  base="$(basename "$spec")"
  case "$base" in
    README.md) continue ;;
    validation-*.md) continue ;;
  esac
  if ! grep -qF "($base)" specs/README.md; then
    printf 'specs/README.md: missing index entry for specs/%s\n' "$base"
    status=1
  fi
done

# --- (b) modules declare their spec ---------------------------------------
# Thin entry points own no behavior of their own.
is_exempt() {
  case "$1" in
    main|lib) return 0 ;;
    *) return 1 ;;
  esac
}

for entry in src/*.rs src/*/; do
  [ -e "$entry" ] || continue
  name="$(basename "$entry")"
  if [ -d "$entry" ]; then
    name="${name%/}"
    file="$entry/mod.rs"
  else
    name="${name%.rs}"
    file="$entry"
  fi
  is_exempt "$name" && continue
  [ -f "$file" ] || { printf '%s: no entry file (expected %s)\n' "$name" "$file"; status=1; continue; }
  if ! head -n "$spec_lines" "$file" | grep -qiE 'specs? [0-9]'; then
    printf '%s: module doc in the first %s lines must reference a spec number (e.g. "spec 04")\n' \
      "$file" "$spec_lines"
    status=1
  fi
done

if [ "$status" -ne 0 ]; then
  printf 'G7 FAILED: specs/ index and src/ modules are out of step\n'
  exit 1
fi

printf 'G7 OK: spec index complete; every top-level module declares its spec\n'
