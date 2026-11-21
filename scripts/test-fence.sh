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
printf '%s\n' "$*" >>"${FAKE_ARGS_LOG:-/dev/null}"
printf '%s\n' "$PWD" >>"${FAKE_CWD_LOG:-/dev/null}"
# bd-8jakq: a targeted test in the TREE sees this uncommitted WIP; the same
# filter in the clean export does not. A mutation that runs tests in the tree
# turns the new test RED.
if [ "$1" = test ] && [ -e broken.rs ]; then
  echo "error: broken.rs is uncommitted WIP in the tree" >&2
  exit 1
fi
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

# bd-8jakq: --test runs the filter inside the export, so another lane's
# uncommitted WIP in the shared tree cannot break it. The same filter in the
# tree fails by construction (the fake cargo sees broken.rs).
printf 'fn broken() {}\n' >"$REPO/broken.rs"
: >"$TMP/cwd.log"
out="$(FAKE_CWD_LOG="$TMP/cwd.log" FENCE_CARGO="$TMP/bin/cargo" \
    bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$GOOD" --test broken_test 2>&1)" \
  || fail "a test filter must run in the clean export: $out"
grep -q 'test (broken_test) in export' <<<"$out" || fail "the export test step must be named: $out"
grep -q '/fence\.' "$TMP/cwd.log" || fail "the test must execute inside the export: $(cat "$TMP/cwd.log")"
pass "--test runs the filter inside the export, not the red tree"

# The same filter in the tree does fail — proving the export leg is the thing
# under test, not a vacuous pass.
(cd "$REPO" && bash "$TMP/bin/cargo" test broken_test >/dev/null 2>&1) \
  && fail "the same filter in the tree must fail on uncommitted WIP"
pass "the same filter fails in the tree"

# --paths overlays a not-yet-staged working-tree path on the revision export.
printf 'fn a_test() {}\n' >"$REPO/a.rs"
FENCE_CARGO="$TMP/bin/cargo" bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$GOOD" \
    --paths a.rs --test a_test >/dev/null 2>&1 \
  || fail "--paths must overlay an unstaged path for the export"
pass "--paths overlays an unstaged path"

# bd-8jakq addition (1): the static test-sandbox guard runs before the build.
printf '#!/usr/bin/env bash\n[ ! -e "%s/sandbox-offense" ] || { echo "test-sandbox: unsandboxed spawn" >&2; exit 1; }\nexit 0\n' "$REPO" >"$TMP/sandbox-check.sh"
chmod +x "$TMP/sandbox-check.sh"
FENCE_SANDBOX_CHECK="$TMP/sandbox-check.sh" FENCE_CARGO="$TMP/bin/cargo" \
    bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$GOOD" >/dev/null 2>&1 \
    || fail "the sandbox guard passes when there is no offense"
touch "$REPO/sandbox-offense"
if FENCE_SANDBOX_CHECK="$TMP/sandbox-check.sh" FENCE_CARGO="$TMP/bin/cargo" \
    bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$GOOD" >/dev/null 2>&1; then
    fail "the fence must refuse an unsandboxed spawn"
fi
rm -f "$REPO/sandbox-offense"
pass "the test-sandbox guard gates the fence"

# bd-8jakq addition (2): --modules derives each changed file's #[cfg(test)]
# modules and runs them in the export. A fixture cmd/spawn.rs with one test
# module must produce that exact filter in the cargo log.
mkdir -p "$REPO/src/cmd"
printf 'fn main() {}\n' >"$REPO/src/main.rs"
printf '#[cfg(test)]\nmod spawn_profile_plan_tests { fn t() {} }\n' >"$REPO/src/cmd/spawn.rs"
: >"$TMP/args.log"
FENCE_CARGO="$TMP/bin/cargo" FAKE_ARGS_LOG="$TMP/args.log" \
    bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$GOOD" \
    --paths src/cmd/spawn.rs --modules >/dev/null 2>&1 \
    || fail "--modules must run the derived module: $(cat "$TMP/args.log")"
grep -q 'cmd::spawn::spawn_profile_plan_tests::' "$TMP/args.log" \
    || fail "the derived module filter must be run: $(cat "$TMP/args.log")"
pass "--modules runs every test module of a changed file"

# bd-250g4: the sweep must never delete another crew's kept export.
SW="$TMP/sweep"
mkdir -p "$SW"
mkdir -p "$SW/fence.OtherPin.AAAA" "$SW/fence.Locked.CCCC" "$SW/fence.Stale.DDDD"
: >"$SW/fence.Locked.CCCC/.fence-running.lock"
touch -t 202001010000 "$SW/fence.Stale.DDDD"
run_sweep() {
    ( export TMPDIR="$SW" FLYWHEEL_FENCE_PIN="$1"
      FENCE_SWEEP_TEST=0 bash "$ROOT/scripts/fence.sh" --repo "$REPO" --sha "$GOOD" --keep \
        >/dev/null 2>&1 ) || fail "sweep hook failed"
}
run_sweep BlackRidge
[ -d "$SW/fence.OtherPin.AAAA" ] || fail "another pin's fresh export was deleted"
[ -d "$SW/fence.Locked.CCCC" ] || fail "an export with a live lock was deleted"
[ ! -d "$SW/fence.Stale.DDDD" ] || fail "a stale export was not swept"
count_blackridge="$(find "$SW" -maxdepth 1 -type d -name 'fence.BlackRidge.*' | wc -l | tr -d ' ')"
[ "$count_blackridge" = 1 ] || fail "expected exactly this pin's current export, got $count_blackridge"
pass "the sweep keeps other pins and locked exports, drops stale ones"

echo "ok"
