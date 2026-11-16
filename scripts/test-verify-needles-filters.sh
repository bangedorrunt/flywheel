#!/usr/bin/env bash
# bd-6setu.18: verify-needles must refuse a TEST_FILTER that matches no #[test]
# fn. A filter naming nothing runs zero tests, so the spec's mutants read
# VACUOUS instead of RED and are silently lost.
#
# Hermetic: a throwaway repo with one live test and a spec that names it, then
# the same spec naming a dead one.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/verify-needles-filters.XXXXXX")"
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

write_spec() {
    cat >scripts/mutations/x.spec.sh <<SPEC
CARGO_ARGS=(-p x --bin x)
TEST_FILTER=($1)
mutation "M1  a needle" \\
    file=src/a/mod.rs \\
    find='assert!(true);' \\
    replace='assert!(false);'
SPEC
}

write_spec live_one
if ! out="$(bash "$ROOT/scripts/verify-needles.sh" --repo "$TMP/repo" scripts/mutations/x.spec.sh 2>&1)"; then
    printf '%s\n' "$out" | tail -5 >&2
    fail "a live TEST_FILTER name must pass"
fi
pass "a live TEST_FILTER name passes"

write_spec dead_one
if out="$(bash "$ROOT/scripts/verify-needles.sh" --repo "$TMP/repo" scripts/mutations/x.spec.sh 2>&1)"; then
    fail "a dead TEST_FILTER name must be refused"
fi
printf '%s\n' "$out" | grep -q "dead_one" \
    || fail "the refusal names the dead filter: $(printf '%s\n' "$out" | tail -3)"
printf '%s\n' "$out" | grep -q "x.spec.sh" \
    || fail "the refusal names the spec: $(printf '%s\n' "$out" | tail -3)"
pass "a dead TEST_FILTER name is refused, naming the spec and the filter"

# A SUBSTRING filter is what libtest takes: a prefix of a live name is fine.
write_spec live_
bash "$ROOT/scripts/verify-needles.sh" --repo "$TMP/repo" scripts/mutations/x.spec.sh >/dev/null 2>&1 \
    || fail "a substring filter that matches a live test must pass"
pass "a substring filter that matches is accepted"
