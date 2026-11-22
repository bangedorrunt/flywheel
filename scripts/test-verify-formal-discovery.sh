#!/usr/bin/env bash
# bd-9idgs: verify-formal discovers work from model/cfg/trace files, so a new
# model needs no edit to the shared script. A mislabelled broken cfg must fail.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/formal-discovery.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

MODELS="$TMP/models"
TRACES="$TMP/traces"
mkdir -p "$MODELS" "$TRACES"

cat >"$MODELS/Fixture.tla" <<'TLA'
---- MODULE Fixture ----
EXTENDS Naturals, TLC
CONSTANTS Bug
VARIABLE x
vars == <<x>>
Init == x = 0
Next == x' = 0
Bad == Bug = "off" /\ x' = 1
Spec == Init /\ [][Next \/ Bad]_vars
TypeOK == x = 0
====
TLA
printf 'SPECIFICATION Spec\nCONSTANT Bug = "none"\nINVARIANT TypeOK\n' >"$MODELS/Fixture.cfg"
printf '\\* expect: TypeOK\nSPECIFICATION Spec\nCONSTANT Bug = "off"\nINVARIANT TypeOK\n' >"$MODELS/Fixture.broken-off.cfg"
printf '{"x":0}\n{"action":"Next","x":0}\n' >"$TRACES/Fixture.recorded.jsonl"
printf '{"x":0}\n{"action":"Next","x":0}\n{"action":"Bad","x":1}\n' >"$TRACES/Fixture.doctored-extra.jsonl"

run() { FLYWHEEL_FORMAL_MODELS_DIR="$MODELS" FLYWHEEL_FORMAL_TRACES_DIR="$TRACES" \
  bash "$ROOT/scripts/verify-formal.sh" 2>&1; }

printf '\\* expect: NotAnInvariant\nSPECIFICATION Spec\nCONSTANT Bug = "off"\nINVARIANT TypeOK\n' \
  >"$MODELS/Fixture.broken-wrong.cfg"
set +e
bad="$(run)"; bad_rc=$?
set -e
[ "$bad_rc" -ne 0 ] || fail "a mislabelled broken cfg must make the script fail"
case "$bad" in
  *"Fixture.broken-wrong"*) ;;
  *) fail "a mislabelled broken cfg must be named: $bad" ;;
esac
pass "a broken cfg naming the wrong invariant fails and names the fixture"

rm "$MODELS/Fixture.broken-wrong.cfg"
set +e
good="$(run)"; good_rc=$?
set -e
[ "$good_rc" -eq 0 ] || fail "the discovery run must pass: $good"
printf '%s\n' "$good" | grep -q 'PASS  Fixture (Fixture)' || fail "positive model not discovered: $good"
printf '%s\n' "$good" | grep -q 'PASS  Fixture (Fixture.broken-off)' || fail "broken cfg not discovered: $good"
printf '%s\n' "$good" | grep -q 'PASS  conform Fixture.recorded.jsonl' || fail "recorded trace not discovered: $good"
printf '%s\n' "$good" | grep -q 'PASS  conform Fixture.doctored-extra.jsonl' || fail "doctored trace not discovered: $good"
pass "a new model, cfg and traces need no script edit"
echo "ok"
