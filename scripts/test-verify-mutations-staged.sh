#!/usr/bin/env bash
# bd-ura2v: verify-mutations' scratch export must overlay staged-only edits,
# not just unstaged/untracked ones. A staged needle that a spec names has to be
# visible in the export or the mutation reads NEEDLE-MISS.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/vm-staged.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT
# bd-08t3o: a proof's shared scratch tree must not leak into a witness that
# verifies the harness owns its OWN export (otherwise the witness is a no-op).
unset VERIFY_MUTATIONS_BUILD_DIR VERIFY_MUTATIONS_EXPORT_ROOT VERIFY_MUTATIONS_IN_EXPORT VERIFY_MUTATIONS_TARGET_DIR

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

REPO="$TMP/repo"
mkdir -p "$REPO"
cd "$REPO"
git init -q .
git config user.email vm@test
git config user.name vm
printf 'original\n' >needle.txt
git add needle.txt
git commit -qm init

listed() { bash "$ROOT/scripts/verify-mutations.sh" --list-overlay "$REPO"; }

# Clean repo: nothing to overlay.
[ -z "$(listed)" ] || fail "a clean repo must have no overlay paths: $(listed)"
pass "a clean repo has no overlay paths"

# Unstaged edit: the old behaviour already saw this.
printf 'unstaged\n' >needle.txt
printf '%s\n' "$(listed)" | grep -qxF 'needle.txt' \
    || fail "an unstaged edit must be overlaid: $(listed)"
pass "an unstaged edit is overlaid"

# The bug: stage the edit and leave the working tree clean relative to the
# index. `git ls-files -m` no longer sees it, but it differs from HEAD.
git add needle.txt
git diff --quiet -- needle.txt || fail "precondition: no unstaged diff remains"
printf '%s\n' "$(listed)" | grep -qxF 'needle.txt' \
    || fail "a staged-only edit must be overlaid: $(listed)"
pass "a staged-only edit is overlaid"

# A staged deletion still names the path so the export can remove it.
git rm -qf needle.txt
printf '%s\n' "$(listed)" | grep -qxF 'needle.txt' \
    || fail "a staged deletion must be overlaid: $(listed)"
pass "a staged deletion is overlaid"

echo "ok"
