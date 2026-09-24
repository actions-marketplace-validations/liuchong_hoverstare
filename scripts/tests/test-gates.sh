#!/usr/bin/env bash
#
# Self-test for the verification gates (spec 17 §6): fixtures with known-good
# and known-bad shapes, asserting both exit codes and the diagnostics the gates
# promise. Run this after touching any gate script.
#
# Usage: scripts/tests/test-gates.sh
# Exit codes: 0 = all cases behaved as specified, 1 = at least one case failed.
set -uo pipefail

cd "$(dirname "$0")/../.." || exit 1

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

pass=0
fail=0

expect_status() { # label expected actual
  if [ "$2" = "$3" ]; then
    printf 'PASS  %s\n' "$1"
    pass=$((pass + 1))
  else
    printf 'FAIL  %s (expected exit %s, got %s)\n' "$1" "$2" "$3"
    fail=$((fail + 1))
  fi
}

expect_contains() { # label file pattern
  if grep -qF -- "$3" "$2"; then
    printf 'PASS  %s\n' "$1"
    pass=$((pass + 1))
  else
    printf 'FAIL  %s (missing "%s" in output)\n' "$1" "$3"
    sed 's/^/      | /' "$2"
    fail=$((fail + 1))
  fi
}

expect_absent() { # label file pattern
  if grep -qF -- "$3" "$2"; then
    printf 'FAIL  %s (unexpected "%s" in output)\n' "$1" "$3"
    sed 's/^/      | /' "$2"
    fail=$((fail + 1))
  else
    printf 'PASS  %s\n' "$1"
    pass=$((pass + 1))
  fi
}

expect_line_count() { # label file expected_lines
  local actual
  actual="$(wc -l < "$2" | tr -d ' ')"
  if [ "$actual" = "$3" ]; then
    printf 'PASS  %s\n' "$1"
    pass=$((pass + 1))
  else
    printf 'FAIL  %s (expected %s lines, got %s)\n' "$1" "$3" "$actual"
    sed 's/^/      | /' "$2"
    fail=$((fail + 1))
  fi
}

expect_count() { # label file pattern expected_count
  local actual
  actual="$(grep -cF -- "$3" "$2" || true)"
  if [ "$actual" = "$4" ]; then
    printf 'PASS  %s\n' "$1"
    pass=$((pass + 1))
  else
    printf 'FAIL  %s (expected %s matches of "%s", got %s)\n' "$1" "$4" "$3" "$actual"
    sed 's/^/      | /' "$2"
    fail=$((fail + 1))
  fi
}

sec() { printf -- '-- %s\n' "$1"; }

# --- G1: action pins --------------------------------------------------------
sec "G1 action pins"
./scripts/verify-action-pins.sh scripts/tests/fixtures/pins/ok.yml >"$tmp/g1-ok.txt" 2>&1
expect_status "G1 accepts fully pinned references (local ref exempt)" 0 "$?"
expect_contains "G1 reports the exempt local reference" "$tmp/g1-ok.txt" "exempt"

./scripts/verify-action-pins.sh scripts/tests/fixtures/pins/float.yml >"$tmp/g1-float.txt" 2>&1
expect_status "G1 rejects floating tags" 1 "$?"
expect_contains "G1 names the offending reference" "$tmp/g1-float.txt" "actions/checkout@v4"
expect_contains "G1 counts every offender" "$tmp/g1-float.txt" "G1 FAILED: 3 of 3"
expect_absent "G1 ignores permission keys resembling uses:" "$tmp/g1-float.txt" "statuses"

./scripts/verify-action-pins.sh scripts/tests/fixtures/pins/sneaky.yml >"$tmp/g1-sneaky.txt" 2>&1
expect_status "G1 rejects quoted / flow-mapping / short-SHA / short-version forms" 1 "$?"
expect_count "G1 reports all five unusual forms" "$tmp/g1-sneaky.txt" "not pinned:" 5

# --- G2: document structure -------------------------------------------------
sec "G2 document structure"
./scripts/check-doc-structure.sh scripts/tests/fixtures/docs/ok-a.md scripts/tests/fixtures/docs/ok-b.md >"$tmp/g2-ok.txt" 2>&1
expect_status "G2 accepts identical structures with different wording" 0 "$?"
expect_contains "G2 ignores headings inside fenced blocks" "$tmp/g2-ok.txt" "seq=22233"

./scripts/check-doc-structure.sh scripts/tests/fixtures/docs/ok-a.md scripts/tests/fixtures/docs/drift.md >"$tmp/g2-drift.txt" 2>&1
expect_status "G2 rejects a missing section" 1 "$?"
expect_contains "G2 reports the missing section as a count difference" "$tmp/g2-drift.txt" "level-2 count differs: reference=3, this=2"

# --- G7: spec index ---------------------------------------------------------
sec "G7 spec index"
./scripts/verify-spec-index.sh >"$tmp/g7.txt" 2>&1
expect_status "G7 accepts the current tree" 0 "$?"

# --- runner -----------------------------------------------------------------
sec "verify-all runner"
./scripts/verify-all.sh --list >"$tmp/list.txt" 2>&1
expect_status "verify-all --list works" 0 "$?"
expect_line_count "verify-all lists seven gates" "$tmp/list.txt" 7

./scripts/verify-all.sh --bogus >"$tmp/bogus.txt" 2>&1
expect_status "verify-all rejects unknown arguments" 2 "$?"

# Every documented flag must terminate. `--strict`/`--full` once fell through to an
# unterminated argument loop: the script spun forever with no output, and only CI's
# job timeout made that visible. A bounded run here catches it in a second.
if command -v timeout >/dev/null 2>&1 || command -v gtimeout >/dev/null 2>&1; then
  bound="$(command -v timeout || command -v gtimeout)"
  "$bound" 120 ./scripts/verify-all.sh --strict >"$tmp/strict.txt" 2>&1
  expect_status "verify-all --strict terminates and passes" 0 "$?"
  # --full is the heavy set: only assert that it terminates, not that it passes
  # (its tools may be absent locally, which --strict-less runs report as SKIP).
  "$bound" 120 ./scripts/verify-all.sh --full >"$tmp/full.txt" 2>&1
  expect_absent "verify-all --full does not hang before its first gate" "$tmp/full.txt" "timed out"
else
  printf 'SKIP  flag termination cases (no timeout binary)\n'
fi

# The tree is expected red until the one-off cleanup lands (G1 pinning, G2 README
# alignment). What is asserted here is tree-independent: every gate that ran
# appears in the summary. A gate that fails without being recorded once slipped
# through this runner, so this case pins the behaviour.
./scripts/verify-all.sh >"$tmp/all.txt" 2>&1
awk '/^-- summary --$/{inside=1; next} inside && /^G[0-9] /{print}' "$tmp/all.txt" >"$tmp/summary.txt"
# The default set is G1/G2/G5/G6/G7 (G3/G4 live behind --full).
expect_line_count "verify-all summary lists every attempted gate" "$tmp/summary.txt" 5
expect_contains "verify-all summary includes G6" "$tmp/summary.txt" "G6"

printf -- '--\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ] || exit 1
