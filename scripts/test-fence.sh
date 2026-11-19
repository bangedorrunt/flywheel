#!/usr/bin/env bash
# bd-koafr follow-up: the fence proves the COMMIT, not the tree. Fixtures only:
# a throwaway repo, stub verify-*.sh and a fake cargo, no real build.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/fence-test.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

REPO="$TMP/repo"
mkdir -p "$REPO/scripts" "$TMP/bin"
git -C "$REPO" init -q
git -C "$REPO" config user.email test@test
git -C "$REPO" config user.name test

# Stub the script steps: real ones need the real tree.
printf '#!/usr/bin/env bash\nexit 0\n' >"$REPO/scripts/verify-needles.sh"
printf '#!/usr/bin/env bash\nexit 0\n' >"$REPO/scripts/verify-clippy.sh"
printf '#!/usr/bin/env bash\nexit 0\n' >"$REPO/scripts/verify-coverage.sh"
printf 'fn main() {}\n' >"$REPO/main.rs"
chmod +x "$REPO/scripts/verify-needles.sh" "$REPO/scripts/verify-clippy.sh" "$REPO/scripts/verify-coverage.sh"

# bd-gmrxv: a pinned sibling, so the fence's sibling step has something to read.
SIB="$TMP/sib"
mkdir -p "$SIB"
git -C "$SIB" init -q
git -C "$SIB" config user.email test@test
git -C "$SIB" config user.name test
printf 'sibling\n' >"$SIB/marker.txt"
git -C "$SIB" add -A
git -C "$SIB" commit -qm sibling
PIN="$(git -C "$SIB" rev-parse HEAD)"
pin_toml() { printf '[[sibling]]\npath = "../sib"\nrev = "%s"\n' "$1" >"$REPO/proof.toml"; }
pin_toml "$PIN"

git -C "$REPO" add -A
git -C "$REPO" commit -qm good
GOOD="$(git -C "$REPO" rev-parse HEAD)"

# Fake cargo: fails when a dead fn reached the EXPORT. That is the -Dwarnings
# build's real job, reduced to a marker so this test stays instant. It also
# models the caller/callee split (bd-4z7mb): a caller in the export without the
# callee that defines what it calls is the split commit, and it never builds.
cat >"$TMP/bin/cargo" <<'SH'
#!/usr/bin/env bash
echo "$1" >>"${FAKE_CARGO_LOG:-/dev/null}"
if [ "$1" = clippy ] && [ -e clippy_only.rs ]; then
  echo "error: very complex type used. Consider factoring parts into \`type\` definitions" >&2
  exit 1
fi
if [ -e dead.rs ]; then
  echo "error: function \`dead\` is never used" >&2
  exit 1
fi
if [ -e caller.rs ] && [ ! -e callee.rs ]; then
  echo "error: cannot find function \`callee\` in this scope" >&2
  exit 1
fi
exit 0
SH
chmod +x "$TMP/bin/cargo"

# A dead fn in the DIRTY tree must not change the answer for a good commit.
printf 'fn dead() {}\n' >"$REPO/dead.rs"
FENCE_CARGO="$TMP/bin/cargo" bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$GOOD" >/dev/null 2>&1 \
  || fail "a dirty-tree dead fn must not fail a good commit"
pass "a dirty tree does not change the answer"

# bd-gmrxv: the same for the PINS. A working-tree proof.toml that names a rev
# nobody has must not change what the fence proves for a commit that pins a
# real one — the fence reads the fenced revision's proof.toml.
pin_toml deadbeefdeadbeef
FENCE_CARGO="$TMP/bin/cargo" bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$GOOD" >/dev/null 2>&1 \
  || fail "a dirty-tree pin must not change what the fence proves"
pass "a dirty proof.toml does not change the fence's siblings"

# ...and the pin is still read where it belongs: COMMITTED at the fenced
# revision, an absent rev is a real failure, not something to ignore.
git -C "$REPO" add proof.toml
git -C "$REPO" commit -qm "absent sibling pin"
BADSIB="$(git -C "$REPO" rev-parse HEAD)"
if FENCE_CARGO="$TMP/bin/cargo" bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$BADSIB" >/dev/null 2>&1; then
  fail "a committed sibling pin must be read and enforced"
fi
pass "a committed sibling pin is enforced"

# bd-4z7mb: the index gate. A caller staged while its callee stays DIRTY is the
# split that breaks HEAD for every lane; the fence of the index refuses it.
pin_toml "$PIN"
git -C "$REPO" add proof.toml
printf 'fn caller() {\n    callee();\n}\n' >"$REPO/caller.rs"
printf 'fn callee() {}\n' >"$REPO/callee.rs"
git -C "$REPO" add caller.rs
out="$(FENCE_CARGO="$TMP/bin/cargo" bash "$ROOT/scripts/fence.sh" --repo "$REPO" --index 2>&1)" \
  && fail "a staged caller without its callee must be refused"
case "$out" in
  *"cannot find function"*) ;;
  *) fail "the refusal must name the build error: $out" ;;
esac
pass "a staged caller whose callee is only dirty is refused, naming the error"

# Staged together, the same change builds: the gate refuses splits, not work.
git -C "$REPO" add callee.rs
out="$(FENCE_CARGO="$TMP/bin/cargo" bash "$ROOT/scripts/fence.sh" --repo "$REPO" --index 2>&1)" \
  || fail "a staged caller WITH its callee must pass: $out"
pass "a staged change that builds on its own passes"

# bd-4z7mb follow-up: the gate runs CLIPPY, so a clippy-only error cannot slip
# past it (f93a8f42 did under the old check leg). The fake cargo fails this only
# when the subcommand really is clippy.
: >"$TMP/cargo-calls"
printf 'fn clippy_only() -> Option<&%s dyn Fn(u8) -> u8> {\n    None\n}\n' "'a" >"$REPO/clippy_only.rs"
git -C "$REPO" add clippy_only.rs
out="$(FAKE_CARGO_LOG="$TMP/cargo-calls" FENCE_CARGO="$TMP/bin/cargo" \
    bash "$ROOT/scripts/fence.sh" --repo "$REPO" --index 2>&1)" \
    && fail "a staged clippy-only error must be refused"
case "$out" in
    *"complex type"*) ;;
    *) fail "the refusal must name the lint: $out" ;;
esac
grep -q '^clippy$' "$TMP/cargo-calls" || fail "the gate must run clippy, not check: $(cat "$TMP/cargo-calls")"
pass "a staged clippy-only error is refused, naming the lint"
git -C "$REPO" rm -q --cached clippy_only.rs
rm -f "$REPO/clippy_only.rs"
git -C "$REPO" rm -q --cached caller.rs callee.rs
rm -f "$REPO/caller.rs" "$REPO/callee.rs"

# The same dead fn COMMITTED must fail.
pin_toml "$PIN"
git -C "$REPO" add -A
git -C "$REPO" commit -qm "dead fn"
BAD="$(git -C "$REPO" rev-parse HEAD)"
if FENCE_CARGO="$TMP/bin/cargo" bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$BAD" >/dev/null 2>&1; then
  fail "a committed dead fn must fail the fence"
fi
pass "a dead fn in the commit fails the fence"

git -C "$REPO" rm -q --cached dead.rs
rm -f "$REPO/dead.rs"

# bd-op4zk: a ledger row citing a mutation the spec no longer declares must die
# at commit time, not 14 minutes into a proof. The stub models verify-coverage's
# ledger-ref leg; the point of this witness is that the FENCE runs that leg.
cat >"$REPO/scripts/verify-coverage.sh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if grep -q 'spec.sh#DEAD' docs/verification/behaviors.toml 2>/dev/null; then
  echo "verify-coverage: bad-row: mutation not declared: scripts/mutations/adr0022-gates.spec.sh#DEAD" >&2
  exit 1
fi
exit 0
SH
chmod +x "$REPO/scripts/verify-coverage.sh"
mkdir -p "$REPO/docs/verification"
printf '[[behavior]]\nid = "bad-row"\nmutations = ["scripts/mutations/adr0022-gates.spec.sh#DEAD"]\n' >"$REPO/docs/verification/behaviors.toml"
git -C "$REPO" add -A
git -C "$REPO" commit -qm "dead ledger row bd-op4zk"
BADLEDGER="$(git -C "$REPO" rev-parse HEAD)"
out="$(FENCE_CARGO="$TMP/bin/cargo" bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$BADLEDGER" 2>&1)" \
  && fail "a committed dead ledger row must fail fence --sha"
case "$out" in
  *"not declared"*) ;;
  *) fail "fence --sha must name the dead ledger row: $out" ;;
esac
pass "a committed dead ledger row fails fence --sha, naming the row"

printf '[[behavior]]\nid = "staged-row"\nmutations = ["scripts/mutations/adr0022-gates.spec.sh#DEAD"]\n' >"$REPO/docs/verification/behaviors.toml"
git -C "$REPO" add docs/verification/behaviors.toml scripts/verify-coverage.sh
out="$(FENCE_CARGO="$TMP/bin/cargo" bash "$ROOT/scripts/fence.sh" --repo "$REPO" --index 2>&1)" \
  && fail "a staged dead ledger row must fail fence --index"
case "$out" in
  *"not declared"*) ;;
  *) fail "fence --index must name the dead ledger row: $out" ;;
esac
pass "a staged dead ledger row fails fence --index, naming the row"

# bd-1umbz: a worktree's pre-commit hook inherits GIT_DIR pointing at the
# worktree's gitdir. `git -C <sibling>` still honours GIT_DIR, so the sibling
# pin archive reads the WRONG repo and dies "rev not found". Simulate that
# env and require the fence to resolve the sibling from its own path.
GIT_DIR="$(git -C "$REPO" rev-parse --absolute-git-dir)" \
FENCE_CARGO="$TMP/bin/cargo" bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$GOOD" >/dev/null 2>&1 \
  || fail "a worktree GIT_DIR must not misresolve the sibling pin"
pass "a worktree GIT_DIR does not misresolve the sibling pin"

echo "ok"
