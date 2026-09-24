#!/usr/bin/env bash
#
# G2 (spec 17): the translated READMEs must share an identical heading
# structure. Translations differ in wording by definition, so this compares
# STRUCTURE (the ordered sequence of level-2/level-3 heading levels) and the
# number of level-2 sections, never the heading text.
#
# Why: translations drift silently when a section is added to one file only.
# Today the English README has 12 level-2 sections while the five translations
# have 11 (the "Contributing" section is missing). No check could see that.
#
# Usage: scripts/check-doc-structure.sh [file...]
#   With no arguments: README.md (reference) + docs/readme/README.*.md
#
# Exit codes: 0 = identical structure, 1 = drift detected, 2 = usage.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  sed -n '2,18p' "$0"
  exit 0
fi

files=()
if [ "$#" -gt 0 ]; then
  files=("$@")
else
  files=(README.md)
  shopt -s nullglob
  files+=(docs/readme/README.*.md)
  shopt -u nullglob
fi

if [ "${#files[@]}" -lt 2 ]; then
  printf 'check-doc-structure: need at least two files to compare\n' >&2
  exit 2
fi

# Heading level sequence, ignoring fenced code blocks (``` ... ```), so that a
# "## ..." line inside an example is not counted as a section.
structure() {
  awk '
    /^[[:space:]]*```/ { fence = !fence; next }
    !fence && /^#{2,3} / { printf "%d", index($0, " ") - 1 }
    END { print "" }
  ' "$1"
}

level2_count() {
  awk '
    /^[[:space:]]*```/ { fence = !fence; next }
    !fence && /^## / { n++ }
    END { print n + 0 }
  ' "$1"
}

first_difference() {
  # 1-based position of the first differing character, or 0 when equal.
  local a="$1" b="$2" n="${#1}" i=1
  while [ "$i" -le "$n" ]; do
    if [ "$(printf '%s' "$a" | cut -c"$i")" != "$(printf '%s' "$b" | cut -c"$i")" ]; then
      printf '%s' "$i"
      return
    fi
    i=$((i + 1))
  done
  printf '0'
}

ref="${files[0]}"
ref_seq="$(structure "$ref")"
ref_l2="$(level2_count "$ref")"
status=0

printf 'G2: reference %s  level2=%s  seq=%s\n' "$ref" "$ref_l2" "$ref_seq"
for f in "${files[@]:1}"; do
  seq="$(structure "$f")"
  l2="$(level2_count "$f")"
  if [ "$seq" = "$ref_seq" ] && [ "$l2" = "$ref_l2" ]; then
    printf 'G2: ok        %s  level2=%s  seq=%s\n' "$f" "$l2" "$seq"
    continue
  fi
  status=1
  if [ "$l2" != "$ref_l2" ]; then
    # The level sequence cannot localise a missing section: every level-2 heading
    # looks identical in it. Say what is actually known instead of pointing at a
    # position that means nothing.
    printf 'G2: DRIFT     %s  level2=%s  seq=%s  (level-2 count differs: reference=%s, this=%s -- a section is missing or extra)\n' \
      "$f" "$l2" "$seq" "$ref_l2" "$l2"
  else
    pos="$(first_difference "$ref_seq" "$seq")"
    [ "$pos" = "0" ] && pos='end'
    printf 'G2: DRIFT     %s  level2=%s  seq=%s  (first difference at heading #%s)\n' \
      "$f" "$l2" "$seq" "$pos"
  fi
done

if [ "$status" -ne 0 ]; then
  printf 'G2 FAILED: README heading structure differs across translations (compare section by section, not wording)\n'
  exit 1
fi

printf 'G2 OK: %s README(s) share one heading structure\n' "${#files[@]}"
