#!/usr/bin/env bash
# bd-feu67: a mutation is RED only when the NAMED test fails. A passing line
# for that test plus a failure in another test must read OTHER, never RED.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/vm-expected-test.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT
unset VERIFY_MUTATIONS_BUILD_DIR VERIFY_MUTATIONS_EXPORT_ROOT VERIFY_MUTATIONS_IN_EXPORT VERIFY_MUTATIONS_TARGET_DIR
export TMPDIR="$TMP"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

mkdir -p "$TMP/repo/src" "$TMP/bin"
cd "$TMP/repo"
git init -q .
git config user.email expected@localhost
git config user.name expected
printf 'original\n' >src/a.rs
cat >spec.sh <<'SPEC'
CARGO_ARGS=(--lib)
TEST_FILTER=(expected_test other_test)
mutation "M1  the guard is gone" \
    file=src/a.rs \
    expect=expected_test \
    find='original' \
    replace='MUTATED'
SPEC
git add src/a.rs spec.sh
git commit -qm init

cat >"$TMP/bin/fakecargo" <<'STUB'
#!/usr/bin/env bash
if grep -q MUTATED src/a.rs 2>/dev/null; then
  if [ "${FAIL_WHICH:-expected}" = "expected" ]; then
    printf 'running 2 tests\ntest expected_test ... FAILED\ntest other_test ... ok\nfailures:\n    expected_test\ntest result: FAILED. 1 passed; 1 failed\n'
  else
    printf 'running 2 tests\ntest expected_test ... ok\ntest other_test ... FAILED\nfailures:\n    other_test\ntest result: FAILED. 1 passed; 1 failed\n'
  fi
  exit 101
fi
printf 'running 2 tests\ntest expected_test ... ok\ntest other_test ... ok\ntest result: ok. 2 passed; 0 failed\n'
STUB
chmod +x "$TMP/bin/fakecargo"

run_case() {
  local which="$1"
  FAIL_WHICH="$which" bash "$ROOT/scripts/verify-mutations.sh" spec.sh \
    --cargo "$TMP/bin/fakecargo" --no-cache --cache-dir "$TMP/cache" 2>&1 || true
}

other="$(run_case other)"
printf '%s\n' "$other" | grep -q 'M1  the guard is gone.*OTHER' \
  || fail "another test's failure was credited RED: $(printf '%s\n' "$other" | tail -8)"
pass "a different test failing reports OTHER"

expected="$(run_case expected)"
printf '%s\n' "$expected" | grep -q 'M1  the guard is gone.*RED' \
  || fail "the named test's failure was not RED: $(printf '%s\n' "$expected" | tail -8)"
pass "the named test failing reports RED"

echo "ok"
