#!/usr/bin/env bash
# Self-test for scripts/clean-proof.sh and scripts/proof-lib.sh (bd-v6j35).
#
# Proves the four mechanisms the clean proof depends on, in throwaway fixtures
# under $TMPDIR — no repo writes, no network:
#   1. a project with no proof.toml declares no siblings
#   2. a pinned sibling rev is what gets archived (not its dirty HEAD)
#   3. a broken tests/ target makes `test --workspace --no-run` fail
#   4. --mutate expectations: RED matches RED, a GREEN mutation fails the gate,
#      and the receipt says "mutations": none when none are given
#
# Usage: bash scripts/proof-selftest.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=proof-lib.sh
. "$ROOT/scripts/proof-lib.sh"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/proof-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
FAILED=0
pass() { echo "  PASS  $*"; }
fail() { echo "  FAIL  $*" >&2; FAILED=1; }

# --- 1. no proof.toml => no siblings ---------------------------------------
repo="$TMP/single"
mkdir -p "$repo"
git -C "$repo" init -q
printf 'hello\n' >"$repo/file.txt"
git -C "$repo" add file.txt
git -C "$repo" -c user.name=t -c user.email=t@t commit -qm one
out="$(proof_siblings "$repo")"
if [ -z "$out" ]; then
  pass "single-repo project declares no siblings"
else
  fail "unexpected siblings for a project without proof.toml: $out"
fi

# --- 2. pinned sibling rev wins over the sibling's working tree -------------
sib="$TMP/sib"
mkdir -p "$sib"
git -C "$sib" init -q
printf 'v1\n' >"$sib/marker.txt"
git -C "$sib" add marker.txt
git -C "$sib" -c user.name=t -c user.email=t@t commit -qm v1
pinned="$(git -C "$sib" rev-parse HEAD)"
printf 'v2\n' >"$sib/marker.txt"
git -C "$sib" -c user.name=t -c user.email=t@t commit -qam v2
printf 'dirty\n' >"$sib/marker.txt" # an uncommitted change must be ignored
dest="$TMP/sib-archive"
if proof_archive "$sib" "$pinned" "$dest" 2>/dev/null && [ "$(cat "$dest/marker.txt")" = "v1" ]; then
  pass "pinned sibling rev is the content archived"
else
  fail "archive did not carry the pinned rev"
fi
if proof_archive "$sib" deadbeefdeadbeef "$dest/missing" 2>/dev/null; then
  fail "an unknown rev must not archive"
else
  pass "unknown sibling rev fails loud"
fi

# --- 3. a broken tests/ target fails the no-run step ------------------------
crate="$TMP/crate"
mkdir -p "$crate/src" "$crate/tests"
cat >"$crate/Cargo.toml" <<'TOML'
[package]
name = "proof-fixture"
version = "0.0.0"
edition = "2021"
[workspace]
TOML
printf 'pub fn ok() -> bool { true }\n' >"$crate/src/lib.rs"
printf '#[test]\nfn sanity() { assert!(proof_fixture::ok()); }\n' >"$crate/tests/good.rs"
if cargo test --manifest-path "$crate/Cargo.toml" --no-run >/dev/null 2>&1; then
  pass "a healthy workspace compiles every target"
else
  fail "healthy workspace failed to compile"
fi
printf '#[test]\nfn broken() { proof_fixture::missing_symbol(); }\n' >"$crate/tests/broken.rs"
if cargo test --manifest-path "$crate/Cargo.toml" --no-run >/dev/null 2>&1; then
  fail "a broken tests/ target must fail test --no-run"
else
  pass "a broken tests/ target fails test --no-run"
fi

# --- 4. mutations are opt-in and expectation-checked ------------------------
src="$TMP/crate/src/lib.rs"
backup="$(proof_apply_mutation "$src" 'true' 'false')" || fail "mutation did not apply"
if grep -q 'pub fn ok() -> bool { false }' "$src"; then
  pass "a mutation edits the target file"
else
  fail "a mutation left the file unchanged"
fi
proof_restore_mutation "$src" "$backup"
if grep -q 'pub fn ok() -> bool { true }' "$src"; then
  pass "the mutation restores the original bytes"
else
  fail "restore did not put the file back"
fi
if proof_apply_mutation "$src" 'text-that-is-absent' 'x' >/dev/null 2>&1; then
  fail "a mutation whose find text is absent must fail"
else
  pass "a mutation with absent find text fails loud"
fi
if proof_mutation_verdict RED RED && ! proof_mutation_verdict GREEN RED; then
  pass "expectation is enforced (RED matches RED, GREEN fails a RED spec)"
else
  fail "mutation expectation logic is wrong"
fi
empty="$TMP/mutations-empty.txt"
: >"$empty"
receipt="$TMP/receipt-none.json"
proof_write_receipt "$receipt" 0 deadbeef dead 20260101T000000Z head "$TMP" "" "build" "0" "" "$empty" >/dev/null
if python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["mutations"]==[]; assert d["ok"] is True; assert d["schema"]=="flywheel-clean-proof.v1"' "$receipt"; then
  pass "a proof without mutations reports an empty mutation list"
else
  fail "receipt does not report an empty mutation list"
fi
# No base is a DISTINCT state from an empty diff: a receipt that ran nothing
# because nobody pinned a base must say so, or a landing cannot tell the two
# apart (bd-m5afp review).
if python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["mutations_base"] is None, d["mutations_base"]; assert d["mutations_mode"] is None; assert d["changed_paths"]==[]' "$receipt"; then
  pass "no base -> receipt writes mutations_base null (not an empty string)"
else
  fail "receipt conflates \"no base\" with a base"
fi
withmut="$TMP/mutations.txt"
printf 'src/x.rs:foo:expect=RED:got=RED\n' >"$withmut"
receipt2="$TMP/receipt-mut.json"
proof_write_receipt "$receipt2" 0 deadbeef dead 20260101T000000Z head "$TMP" "" "build" "0" "" "$withmut" >/dev/null
if python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["mutations"]==["src/x.rs:foo:expect=RED:got=RED"]' "$receipt2"; then
  pass "a mutated proof records each witness in the receipt"
else
  fail "receipt does not record mutation witnesses"
fi

# A changed-mode receipt carries the mode, the base and the diff it selected
# from, so a landing can re-derive the coverage itself.
receipt3="$TMP/receipt-changed.json"
PROOF_MUTATIONS_MODE=changed PROOF_MUTATIONS_BASE=deadbeef \
  PROOF_CHANGED_PATHS="docs/a.mdx
scripts/b.sh" \
  proof_write_receipt "$receipt3" 0 deadbeef dead 20260101T000000Z sha "$TMP" "" "mutations" "0" "" "$empty" >/dev/null
if python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["mutations_mode"]=="changed"; assert d["mutations_base"]=="deadbeef"; assert d["changed_paths"]==["docs/a.mdx","scripts/b.sh"]' "$receipt3"; then
  pass "a changed-mode receipt records mode, base and the selected diff"
else
  fail "changed-mode receipt does not record how the set was chosen"
fi

if [ "$FAILED" -ne 0 ]; then
  echo "proof-selftest: FAILED" >&2
  exit 1
fi
echo "proof-selftest: ok"
