#!/usr/bin/env bash
#
# G1 (spec 17): every external `uses:` reference in the published composite
# action (action.yml) and in this repository's own workflows must be pinned to
# a full 40-hex commit SHA with a trailing "# vX.Y.Z" version comment.
#
# Why it matters here, concretely:
#   - action.yml is consumed by users. When a user pins liuchong/hoverstare to
#     a commit, a floating tag inside the action still changes what runs.
#   - .github/workflows/hoverstare.yml (dogfood) carries the App private key,
#     a PAT and the GPG signing key; release.yml carries a contents:write
#     token. A moved upstream tag there has credential blast radius.
#
# Usage: scripts/verify-action-pins.sh [file...]
#   With no arguments: action.yml + .github/workflows/*.yml|*.yaml
#
# Exit codes: 0 = all pinned, 1 = at least one non-pinned reference, 2 = usage.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  sed -n '2,20p' "$0"
  exit 0
fi

files=()
if [ "$#" -gt 0 ]; then
  files=("$@")
else
  [ -f action.yml ] && files+=(action.yml)
  shopt -s nullglob
  files+=(.github/workflows/*.yml .github/workflows/*.yaml)
  shopt -u nullglob
fi

# Lines that introduce a `uses:` directive. Anchored at the start of the line so
# that unrelated keys containing the substring (e.g. "statuses: write") are not
# mistaken for a reference.
# Also matches the unusual spellings a human might reach for -- list dash into a
# flow mapping ("- {uses: ...}") and quoted keys ("uses": / 'uses':) -- so those
# land on the check and fail closed instead of being silently skipped.
directive='^[[:space:]]*(-[[:space:]]+)?(\{[[:space:]]*)?["'\'']?uses["'\'']?[[:space:]]*:'
# The only accepted form: optional list dash, `uses:`, owner/repo@<40 hex>, and a
# strict single trailing comment token. The token names the ref the SHA came from:
# `# vX.Y.Z` for tagged upstreams, or a branch/manifest-style name (`# stable`,
# `# 1.83.0`) for upstreams that only publish branch refs. Anything unusual
# (quotes, flow mappings, short SHAs, trailing text, several tokens) fails closed
# instead of being guessed at. The pin itself is always the SHA.
pinned='^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]+[A-Za-z0-9_.-]+/[A-Za-z0-9_./-]+@[0-9a-f]{40}[[:space:]]+#[[:space:]]*(v[0-9]+\.[0-9]+\.[0-9]+|[0-9]+\.[0-9]+\.[0-9]+|.+-.+|[[:alpha:]][[:alnum:]_.-]*)[[:space:]]*$'
# A bare major version says nothing about which release was pinned, so it is
# rejected even though it matches the token shape above.
bare_major='#[[:space:]]*v[0-9]+[[:space:]]*$'
# Local (in-repo) references are exempt: they are pinned by the checkout itself.
local_ref='^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]*\./'

bad=0
total=0
local=0

for f in "${files[@]}"; do
  [ -f "$f" ] || continue
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    line_no="${hit%%:*}"
    text="${hit#*:}"
    total=$((total + 1))
    if printf '%s\n' "$text" | grep -qE "$local_ref"; then
      local=$((local + 1))
      continue
    fi
    if ! printf '%s\n' "$text" | grep -qE "$pinned" \
      || printf '%s\n' "$text" | grep -qE "$bare_major"; then
      trimmed="$(printf '%s' "$text" | sed 's/^[[:space:]]*//')"
      printf '%s:%s: not pinned: %s\n' "$f" "$line_no" "$trimmed"
      bad=$((bad + 1))
    fi
  done < <(grep -nE "$directive" "$f" || true)
done

if [ "$bad" -gt 0 ]; then
  printf 'G1 FAILED: %s of %s external reference(s) are not pinned to a full SHA with a single source comment\n' "$bad" "$total"
  printf '           pin the commit SHA and name the ref in one comment token (# vX.Y.Z, or # stable).\n'
  exit 1
fi

suffix=""
if [ "$local" -gt 0 ]; then
  suffix=" ($local local reference(s) exempt)"
fi
printf 'G1 OK: %s external reference(s) pinned%s\n' "$total" "$suffix"
