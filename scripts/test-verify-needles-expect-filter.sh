#!/usr/bin/env bash
# bd-m97cj: verify-needles must refuse a mutation whose expect= test is not
# selected by its own TEST_FILTER. Without that check the mutant reads VACUOUS
# and proves nothing (proof62 read doorbell-already-pulled R3 VACUOUS).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/verify-needles-expect.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

mkdir -p "$TMP/repo/src/a" "$TMP/repo/scripts/mutations"
cd "$TMP/repo"
: >"src/a.rs"
cat >src/a/mod.rs <<'RS'
#[test]
fn live_one() {
    assert!(true);
}
RS

write_spec() { # $1 filter, $2 expect
    cat >scripts/mutations/x.spec.sh <<SPEC
CARGO_ARGS=(-p x --bin x)
TEST_FILTER=($1)
mutation "M1  a needle" \\
    file=src/a/mod.rs \\
    find='assert!(true);' \\
    replace='assert!(false);' \\
    expect=$2
SPEC
}

check() { bash "$ROOT/scripts/verify-needles.sh" --repo "$TMP/repo" scripts/mutations/x.spec.sh 2>&1; }

write_spec live_one live_one
if ! out="$(check)"; then
    printf '%s\n' "$out" | tail -5 >&2
    fail "an expect selected by TEST_FILTER must pass"
fi
pass "a selected expect passes"

# libtest filters by substring: a prefix of the expect name selects it.
write_spec live_ live_one
check >/dev/null 2>&1 || fail "a substring filter that selects the expect must pass"
pass "a substring-selected expect passes"

# The bug: expect is not selected at all — the mutant can never go RED.
write_spec unrelated_filter live_one
if out="$(check)"; then
    fail "an expect outside TEST_FILTER must be refused"
fi
printf '%s\n' "$out" | grep -q 'live_one' \
    || fail "the refusal names the expect: $(printf '%s\n' "$out" | tail -3)"
printf '%s\n' "$out" | grep -q 'x.spec.sh' \
    || fail "the refusal names the spec: $(printf '%s\n' "$out" | tail -3)"
pass "an unselected expect is refused, naming the spec and the expect"

echo "ok"
