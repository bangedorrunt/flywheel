#!/usr/bin/env bash
# bd-o2uol: the live runner diagnoses a dangling `target` symlink once, before
# running any suite, instead of reporting five misleading cargo failures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/e2e-target-test.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

check() { # dir -> rc, captures lines in OUT
    OUT="$(bash "$ROOT/scripts/e2e-live-five.sh" --check-target "$1" 2>&1)"
    return $?
}

# No target at all: usable (cargo will create it).
mkdir -p "$TMP/plain"
check "$TMP/plain" || fail "a missing target must not trip the preflight"
[ -z "$OUT" ] || fail "unexpected diagnosis for a plain dir: $OUT"
pass "a repo without a target link passes"

# A real directory target: usable.
mkdir -p "$TMP/real/target"
check "$TMP/real" || fail "a real target dir must pass"
[ -z "$OUT" ] || fail "unexpected diagnosis for a real dir: $OUT"
pass "a real target directory passes"

# A symlink to a directory that still exists: usable.
mkdir -p "$TMP/dest" "$TMP/linked"
ln -s "$TMP/dest" "$TMP/linked/target"
check "$TMP/linked" || fail "a live symlink must pass"
[ -z "$OUT" ] || fail "unexpected diagnosis for a live symlink: $OUT"
pass "a symlink to an existing destination passes"

# The bug: symlink to a destination the mbx cleanup removed.
mkdir -p "$TMP/dangling"
ln -s "$TMP/gone-cache" "$TMP/dangling/target"
if check "$TMP/dangling"; then
    fail "a dangling target symlink must be refused"
fi
lines="$(printf '%s\n' "$OUT" | grep -c . || true)"
[ "$lines" -eq 1 ] || fail "expected exactly one diagnosis line, got $lines: $OUT"
printf '%s\n' "$OUT" | grep -q 'dangling symlink' || fail "diagnosis must say dangling: $OUT"
printf '%s\n' "$OUT" | grep -qF "$TMP/gone-cache" || fail "diagnosis must name the destination: $OUT"
pass "a dangling target link is reported once with its destination"

echo "ok"
