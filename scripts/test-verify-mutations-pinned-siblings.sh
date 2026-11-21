#!/usr/bin/env bash
# bd-5j8ri: verify-mutations must build against the siblings pinned in the
# proof.toml of the revision under test, never the live dirty ../toron. A
# sibling working tree broken by another lane must not change a mutant verdict.
# Hermetic: a throwaway repo, a fake cargo, no real build or network.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/verify-mutations-pinned.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT
# bd-08t3o: a proof's shared scratch tree must not leak into a witness that
# verifies the harness owns its OWN export (otherwise the witness is a no-op).
unset VERIFY_MUTATIONS_BUILD_DIR VERIFY_MUTATIONS_EXPORT_ROOT VERIFY_MUTATIONS_IN_EXPORT VERIFY_MUTATIONS_TARGET_DIR
export TMPDIR="$TMP"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

WORK="$TMP/work"
mkdir -p "$WORK/toron" "$WORK/repo/src" "$WORK/bin"

# The pinned sibling: one committed marker, then a broken working tree.
(
  cd "$WORK/toron"
  git init -q .
  git config user.email pinned@localhost
  git config user.name pinned
  printf 'pinned-v1\n' >marker.txt
  git add marker.txt
  git commit -qm pinned
)
PIN="$(git -C "$WORK/toron" rev-parse HEAD)"
printf 'live-dirty\n' >"$WORK/toron/marker.txt"
printf 'this is not toml ((((\n' >"$WORK/toron/dirty.rs"

# The repo under test pins that sibling rev.
(
  cd "$WORK/repo"
  git init -q .
  git config user.email test@localhost
  git config user.name test
  cat >proof.toml <<EOF
[[sibling]]
path = "../toron"
rev = "$PIN"
EOF
  printf 'original\n' >src/a.rs
  cat >spec.sh <<'SPEC'
CARGO_ARGS=(--lib)
TEST_FILTER=(some_test)
mutation "M1  the guard is gone" \
    file=src/a.rs \
    expect=some_test \
    find='original' \
    replace='MUTATED'
SPEC
  git add -A
  git commit -qm init
)

# The fake cargo reports the mutant RED only when it is built in an export whose
# ../toron is a real directory carrying the PINNED marker and no dirty file.
cat >"$WORK/bin/fakecargo" <<'STUB'
#!/usr/bin/env bash
sibling_ok=no
if [ ! -L ../toron ] && [ -f ../toron/marker.txt ] &&
   [ "$(cat ../toron/marker.txt)" = "pinned-v1" ] && [ ! -e ../toron/dirty.rs ]; then
  sibling_ok=yes
fi
if [ "$sibling_ok" != yes ]; then
  # A live/dirty sibling: force VACUOUS so the harness itself refuses the run.
  printf 'running 1 test\ntest result: ok. 1 passed; 0 failed\n'
  exit 0
fi
if grep -q MUTATED src/a.rs 2>/dev/null; then
  printf 'running 1 test\ntest result: FAILED. 0 passed; 1 failed\nfailures:\n    some_test\n'
  exit 101
fi
printf 'running 1 test\ntest result: ok. 1 passed; 0 failed\n'
STUB
chmod +x "$WORK/bin/fakecargo"

out="$(cd "$WORK/repo" && bash "$ROOT/scripts/verify-mutations.sh" spec.sh --cargo "$WORK/bin/fakecargo" 2>&1)" \
  || { printf '%s\n' "$out" | tail -25 >&2; fail "the harness refused a run with a broken live sibling"; }
printf '%s\n' "$out" | grep -q RED || fail "the mutant did not read RED against the pinned sibling"
pass "a broken live ../toron does not change the mutant verdict"

echo "ok"
