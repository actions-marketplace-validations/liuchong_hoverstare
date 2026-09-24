#!/usr/bin/env bash
#
# Verification gates runner (spec 17). One command to run every engineering
# gate; CI wires the same entry point.
#
#   default gates : G1 action pins, G2 doc structure, G6 workflow/script lint,
#                   G7 spec index (all fast, no extra toolchain)
#   --full adds   : G3 dependency + license audit, G4 coverage not regressed,
#                   G5 secret scan
#   --strict      : a gate whose tool is missing counts as FAILED (CI uses this)
#   --list        : print the gate list and exit
#
# Gate scripts live next to this file; each prints its own diagnostics and
# returns 1 on failure.
#
# Exit codes: 0 = all gates passed (or skipped without --strict),
#             1 = at least one gate failed, 2 = usage error.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

full=0
strict=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --full) full=1 ;;
    --strict) strict=1 ;;
    --list)
      printf 'G1 action pins        scripts/verify-action-pins.sh\n'
      printf 'G2 doc structure      scripts/check-doc-structure.sh\n'
      printf 'G3 dependency audit   cargo deny check + cargo audit\n'
      printf 'G4 coverage floor     cargo llvm-cov --fail-under-lines (scripts/coverage-baseline.txt)\n'
      printf 'G5 secret scan        gitleaks detect\n'
      printf 'G6 workflow/script lint actionlint + shellcheck\n'
      printf 'G7 spec index         scripts/verify-spec-index.sh\n'
      exit 0
      ;;
    -h|--help)
      sed -n '2,18p' "$0"
      exit 0
      ;;
    *)
      printf 'verify-all: unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

names=()
results=()
detail=()

record() { # name result detail
  names+=("$1")
  results+=("$2")
  detail+=("$3")
}

skip_or_fail() { # gate message hint
  if [ "$strict" -eq 1 ]; then
    record "$1" FAIL "missing tool: $3"
  else
    record "$1" SKIP "install with: $3"
  fi
  printf '%s: SKIP (%s); install with: %s\n' "$1" "$2" "$3"
}

have() { command -v "$1" >/dev/null 2>&1; }

# --- G1: external action references are pinned -----------------------------
run_g1() {
  if ./scripts/verify-action-pins.sh; then record G1 PASS ""; else record G1 FAIL "run scripts/verify-action-pins.sh"; fi
}

# --- G2: README heading structure ------------------------------------------
run_g2() {
  if ./scripts/check-doc-structure.sh; then record G2 PASS ""; else record G2 FAIL "run scripts/check-doc-structure.sh"; fi
}

# --- G3: dependency and license audit --------------------------------------
run_g3() {
  local ok=0 missing=()
  if have cargo-deny; then
    printf -- '--- G3: cargo deny check\n'
    cargo deny check || ok=1
  else
    missing+=("cargo-deny")
  fi
  if have cargo-audit; then
    printf -- '--- G3: cargo audit\n'
    cargo audit || ok=1
  else
    missing+=("cargo-audit")
  fi
  if [ "${#missing[@]}" -gt 0 ]; then
    if [ "$ok" -ne 0 ]; then
      record G3 FAIL "audit reported findings"
    else
      skip_or_fail G3 "not installed: ${missing[*]}" "cargo install --locked ${missing[*]}"
    fi
    return
  fi
  if [ "$ok" -eq 0 ]; then record G3 PASS ""; else record G3 FAIL "audit reported findings"; fi
}

# --- G4: coverage does not regress ----------------------------------------
run_g4() {
  local baseline=scripts/coverage-baseline.txt
  if [ ! -f "$baseline" ]; then
    skip_or_fail G4 "no coverage baseline yet" "create $baseline (see spec 17 G4)"
    return
  fi
  if ! have cargo-llvm-cov; then
    skip_or_fail G4 "cargo-llvm-cov not installed" "cargo install --locked cargo-llvm-cov"
    return
  fi
  local min
  min="$(grep -E '^min_lines=' "$baseline" | head -1 | cut -d= -f2)"
  if [ -z "$min" ]; then
    record G4 FAIL "$baseline has no min_lines= entry"
    return
  fi
  printf -- '--- G4: cargo llvm-cov --fail-under-lines %s\n' "$min"
  if cargo llvm-cov --workspace --fail-under-lines "$min"; then record G4 PASS ""; else record G4 FAIL "coverage below $min%"; fi
}

# --- G5: secret scan -------------------------------------------------------
run_g5() {
  if ! have gitleaks; then
    skip_or_fail G5 "gitleaks not installed" "brew install gitleaks"
    return
  fi
  local args=(detect --no-banner --redact)
  [ -f .gitleaks.toml ] && args+=(--config .gitleaks.toml)
  printf -- '--- G5: gitleaks %s\n' "${args[*]}"
  if gitleaks "${args[@]}"; then record G5 PASS ""; else record G5 FAIL "gitleaks reported findings"; fi
}

# --- G6: workflow and script lint -----------------------------------------
run_g6() {
  local ok=0 missing=()
  if have actionlint; then
    printf -- '--- G6: actionlint\n'
    actionlint -color -shellcheck= || ok=1
  else
    missing+=("actionlint")
  fi
  if have shellcheck; then
    printf -- '--- G6: shellcheck scripts\n'
    shells=()
    while IFS= read -r sh_file; do
      [ -n "$sh_file" ] && shells+=("$sh_file")
    done < <(ls scripts/*.sh scripts/tests/*.sh 2>/dev/null)
    if [ "${#shells[@]}" -gt 0 ]; then
      shellcheck "${shells[@]}" || ok=1
    fi
  else
    missing+=("shellcheck")
  fi
  if [ "${#missing[@]}" -gt 0 ]; then
    if [ "$ok" -ne 0 ]; then
      record G6 FAIL "lint reported findings"
    else
      skip_or_fail G6 "not installed: ${missing[*]}" "brew install ${missing[*]}"
    fi
    return
  fi
  if [ "$ok" -eq 0 ]; then record G6 PASS ""; else record G6 FAIL "lint reported findings"; fi
}

# --- G7: spec index and module/spec consistency ---------------------------
run_g7() {
  if ./scripts/verify-spec-index.sh; then record G7 PASS ""; else record G7 FAIL "run scripts/verify-spec-index.sh"; fi
}

printf '== HoverStare verification gates (spec 17)%s ==\n' "$( [ "$full" -eq 1 ] && printf ', --full' )"
run_g1
run_g2
run_g7
run_g6
if [ "$full" -eq 1 ]; then
  run_g3
  run_g4
  run_g5
fi

failed=0
skipped=0
printf -- '-- summary --\n'
for i in "${!names[@]}"; do
  printf '%-4s %-8s %s\n' "${names[$i]}" "${results[$i]}" "${detail[$i]}"
  case "${results[$i]}" in
    FAIL) failed=$((failed + 1)) ;;
    SKIP) skipped=$((skipped + 1)) ;;
  esac
done

if [ "$failed" -gt 0 ]; then
  printf '== %s gate(s) failed, %s skipped ==\n' "$failed" "$skipped"
  exit 1
fi
printf '== all gates passed (%s skipped) ==\n' "$skipped"
