#!/usr/bin/env bash
# bd-b2yg7: a landing must scope mutations from the newest green receipt, not a
# stale pin, and every spec's export must share ONE proof-owned target dir.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/proof-inputs.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT
# bd-08t3o: a proof's shared scratch tree must not leak into a witness that
# verifies the harness owns its OWN export (otherwise the witness is a no-op).
unset VERIFY_MUTATIONS_BUILD_DIR VERIFY_MUTATIONS_EXPORT_ROOT VERIFY_MUTATIONS_IN_EXPORT VERIFY_MUTATIONS_TARGET_DIR

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# --- the automatic base is the newest GREEN ancestor receipt -----------------
REPO="$TMP/repo"
git init -q "$REPO"
git -C "$REPO" config user.email proof@test
git -C "$REPO" config user.name proof
for n in 1 2 3 4 5; do
  printf 'commit %s\n' "$n" >"$REPO/f.txt"
  git -C "$REPO" add f.txt
  git -C "$REPO" commit -qm "c$n"
  git -C "$REPO" rev-parse HEAD >>"$TMP/shas"
done
C1=$(sed -n 1p "$TMP/shas")
C2=$(sed -n 2p "$TMP/shas")
C3=$(sed -n 3p "$TMP/shas")
C4=$(sed -n 4p "$TMP/shas")
C5=$(sed -n 5p "$TMP/shas")

ARCHIVE="$TMP/receipts"
mkdir -p "$ARCHIVE"
# shellcheck source=proof-lib.sh
. "$ROOT/scripts/proof-lib.sh"

receipt() { # $1 sha, $2 ok, $3 stamp (PROOF_STAMP is compact ISO)
  # Real receipts (c57eaa28, 5e9c01e6, ...) carry a changed-mode set mixing RED
  # with DEAD-CONFIRMED rows; both verdicts are verified evidence (bd-r7852).
  printf '{"ok":%s,"sha":"%s","stamp":"%s","mutations_mode":"changed","mutations":[{"verdict":"RED"},{"verdict":"RED"},{"verdict":"DEAD-CONFIRMED"}]}\n' \
    "$2" "$1" "$3" >"$ARCHIVE/$1.json"
}
receipt "$C2" true "20261118T041900Z"
[ "$(proof_green_base "$REPO" "$C5" "$ARCHIVE")" = "$C2" ] \
  || fail "a green ancestor receipt with a REAL ISO stamp must become the base"
pass "one green ancestor with an ISO stamp becomes the base"

# A newer green ancestor wins; a red receipt and a non-ancestor cannot.
receipt "$C3" true "20261118T041942Z"
receipt "$C4" false "20261118T041959Z"
receipt "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" true "20261118T042000Z"
[ "$(proof_green_base "$REPO" "$C5" "$ARCHIVE")" = "$C3" ] \
  || fail "the newest green ancestor must win (got $(proof_green_base "$REPO" "$C5" "$ARCHIVE"))"
pass "newest green ancestor wins; red and non-ancestor ignored"

# bd-r7852: a green receipt whose mutation set was NOT verified must not move
# the base. A no-mutation witnesses-mode receipt alone cannot vouch for the
# diff; only a witnessed land marker written by ship land can.
printf '{"ok":true,"sha":"%s","stamp":"20261119T010000Z","mutations_mode":"witnesses","mutations":[]}\n' \
  "$C4" >"$ARCHIVE/$C4.json"
[ "$(proof_green_base "$REPO" "$C5" "$ARCHIVE")" = "$C3" ] \
  || fail "an unverified no-mutation receipt must not become the base (got $(proof_green_base "$REPO" "$C5" "$ARCHIVE"))"
pass "no-mutation witnesses receipt cannot advance the base"

printf '{"ok":true,"sha":"%s","stamp":"20261119T020000Z","source":"ship-land","witnessed":true,"bead":"bd-x"}\n' \
  "$C4" >"$ARCHIVE/witnessed-$C4.json"
[ "$(proof_green_base "$REPO" "$C5" "$ARCHIVE")" = "$C4" ] \
  || fail "a landed witness-mode receipt must become the base (got $(proof_green_base "$REPO" "$C5" "$ARCHIVE"))"
pass "landed witness marker advances the base"

# A manual pin is still an explicit override when present.
printf '[mutations]\nbase = "%s"\n' "$C1" >"$REPO/proof.toml"
[ "$(proof_mutation_base "$REPO")" = "$C1" ] \
  || fail "a manual mutations base must still be readable as an override"
pass "manual pin remains an override"

# --- one warm target dir across mutation exports -----------------------------
SANDBOX="$TMP/target-test"
mkdir -p "$SANDBOX/repo/src" "$SANDBOX/bin"
cd "$SANDBOX/repo"
git init -q .
git config user.email target@test
git config user.name target
printf 'original\n' >src/a.rs
cat >spec.sh <<'SPEC'
CARGO_ARGS=(--lib)
TEST_FILTER=(some_test)
mutation "M1  a decision" file=src/a.rs expect=some_test find=original replace=MUTATED
SPEC
git add src/a.rs spec.sh
git commit -qm init
cat >"$SANDBOX/bin/fakecargo" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "${CARGO_TARGET_DIR:-}" >>"${SEE_TARGET_FILE:?}"
if grep -q MUTATED src/a.rs 2>/dev/null; then
  printf 'running 1 test\ntest result: FAILED. 0 passed; 1 failed\nfailures:\n    some_test\n'
  exit 101
fi
printf 'running 1 test\ntest result: ok. 1 passed; 0 failed\n'
STUB
chmod +x "$SANDBOX/bin/fakecargo"

PROOF_TARGET="$SANDBOX/proof-target"
: >"$SANDBOX/seen"
for _ in 1 2; do
  nested_out="$(cd "$SANDBOX/repo" && VERIFY_MUTATIONS_TARGET_DIR="$PROOF_TARGET" \
    SEE_TARGET_FILE="$SANDBOX/seen" \
    bash "$ROOT/scripts/verify-mutations.sh" spec.sh --cargo "$SANDBOX/bin/fakecargo" 2>&1)" \
    || fail "a mutation run failed: $(printf '%s\n' "$nested_out" | tail -12)"
done
seen_count=$(grep -c . "$SANDBOX/seen" || true)
[ "$seen_count" -ge 2 ] || fail "the fake cargo never saw a target dir"
uniq_count=$(sort -u "$SANDBOX/seen" | grep -c . || true)
[ "$uniq_count" -eq 1 ] && [ "$(head -1 "$SANDBOX/seen")" = "$PROOF_TARGET" ] \
  || fail "exports used different target dirs: $(sort -u "$SANDBOX/seen")"
pass "every mutation export reuses one proof-owned target dir"

echo "ok"
