#!/usr/bin/env bash
# bd-xwn96: prove the proof lever's three new behaviours without a full proof.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/proof-mutation-failfast.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT
# bd-08t3o: a proof's shared scratch tree must not leak into a witness that
# verifies the harness owns its OWN export (otherwise the witness is a no-op).
unset VERIFY_MUTATIONS_BUILD_DIR VERIFY_MUTATIONS_EXPORT_ROOT VERIFY_MUTATIONS_IN_EXPORT VERIFY_MUTATIONS_TARGET_DIR

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# --- 1. fail-fast decision is pure and wired into the spec loop -----------
. "$ROOT/scripts/proof-lib.sh"
proof_mutation_stops 0 0 && fail "a green spec must not stop the stage"
proof_mutation_stops 1 1 && fail "--mutations-keep-going must not stop"
proof_mutation_stops 1 0 || fail "a failing spec must stop the stage"
pass "stop decision: only (failing, no keep-going) stops"

grep -qF 'proof_mutation_stops "$mut_status" "$MUTATIONS_KEEP_GOING"' "$ROOT/scripts/clean-proof.sh" \
  || fail "clean-proof does not call the stop decision"
grep -qF -- '--mutations-keep-going)' "$ROOT/scripts/clean-proof.sh" \
  || fail "clean-proof does not accept --mutations-keep-going"
grep -qF 'PROOF_MUTATION_FAILED_SPEC=' "$ROOT/scripts/clean-proof.sh" \
  || fail "clean-proof does not name the failing spec to the receipt"
pass "fail-fast flag, break and receipt handoff are wired"

PROOF_OK=0 PROOF_SHA=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa PROOF_SHORT=aaaaaaaa \
PROOF_STAMP=20261121T000000Z PROOF_MODE=sha PROOF_FAILED=mutations \
PROOF_MUTATION_FAILED_SPEC=scripts/mutations/seeded.spec.sh \
PROOF_STEPS=mutations PROOF_STATUSES=1 PROOF_SIBLINGS= \
PROOF_MUTATIONS_FILE="" \
  bash -c '. "$1/scripts/proof-lib.sh"; proof_write_receipt "$2" "$PROOF_OK" "$PROOF_SHA" "$PROOF_SHORT" "$PROOF_STAMP" "$PROOF_MODE" "$3" "$PROOF_FAILED" "$PROOF_STEPS" "$PROOF_STATUSES" "$PROOF_SIBLINGS" "$PROOF_MUTATIONS_FILE"' \
  _ "$ROOT" "$TMP/receipt.json" "$TMP" || fail "could not write a receipt"
python3 - "$TMP/receipt.json" <<'PY' || fail "receipt did not name the failing spec"
import json, sys
assert json.load(open(sys.argv[1]))["failed_mutation_spec"] == "scripts/mutations/seeded.spec.sh"
PY
pass "the receipt names the spec that stopped the stage"

# --- 2. crew witness writes the key clean-proof will look up --------------
mkdir -p "$TMP/repo/scripts" "$TMP/repo/src" "$TMP/bin"
cd "$TMP/repo"
git init -q .
git config user.email witness@localhost
git config user.name witness
printf 'original\n' >src/a.rs
cat >spec.sh <<'SPEC'
CARGO_ARGS=(--lib)
TEST_FILTER=(some_test)
mutation "C1  the guard is gone" \
    file=src/a.rs \
    expect=some_test \
    find='original' \
    replace='MUTATED'
SPEC
cp "$ROOT/scripts/verify-mutations.sh" "$ROOT/scripts/proof-lib.sh" "$ROOT/scripts/mutation-cache.py" scripts/
git add src/a.rs spec.sh scripts/*
git commit -qm seed

cat >"$TMP/bin/fakecargo" <<'STUB'
#!/usr/bin/env bash
if grep -q MUTATED src/a.rs 2>/dev/null; then
  printf 'running 1 test\ntest result: FAILED. 0 passed; 1 failed\nfailures:\n    some_test\n'
  exit 101
fi
printf 'running 1 test\ntest result: ok. 1 passed; 0 failed\n'
STUB
chmod +x "$TMP/bin/fakecargo"

CACHE="$TMP/cache"
out1="$(bash "$ROOT/scripts/verify-mutations.sh" spec.sh --cargo "$TMP/bin/fakecargo" --cache-dir "$CACHE" 2>&1)" \
  || { printf '%s\n' "$out1" | tail -20 >&2; fail "first witness run failed"; }
printf '%s\n' "$out1" | grep -q '1/1 mutations accounted for' || fail "first run did not prove its row"
[ -n "$(find "$CACHE" -maxdepth 1 -name '*.json' -print -quit)" ] \
  || fail "crew run did not write a verdict-cache entry"
key="$(python3 scripts/mutation-cache.py key --root "$TMP/repo" --spec spec.sh)"
[ -f "$CACHE/$key.json" ] || { echo "cache files:" >&2; find "$CACHE" -type f >&2; echo "wanted key $key" >&2; fail "crew entry is not under clean-proof's key"; }
pass "crew witness cached under the proof's content key"

out2="$(bash "$ROOT/scripts/verify-mutations.sh" spec.sh --cargo "$TMP/bin/fakecargo" --cache-dir "$CACHE" 2>&1)" \
  || { printf '%s\n' "$out2" | tail -20 >&2; fail "cached run failed"; }
printf '%s\n' "$out2" | grep -q 'CACHED' || fail "second run did not hit the cache"
pass "a second crew run hits the cache"
