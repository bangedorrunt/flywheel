#!/usr/bin/env bash
# bd-kszuj: clean-proof proves the COMMIT, not the shared tree. The export step
# is proof_archive (git archive <rev> | tar -x); a dirty tree never reaches it.
# Fixtures only: a throwaway repo, no cargo, no network, no live stack.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/clean-proof-export.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

REPO="$TMP/repo"
DEST="$TMP/export"
mkdir -p "$REPO" "$DEST"
git -C "$REPO" init -q
git -C "$REPO" config user.email test@test
git -C "$REPO" config user.name test
printf 'committed value\n' >"$REPO/keep.txt"
mkdir -p "$REPO/src"; printf 'mod committed\n' >"$REPO/src/lib.rs"
git -C "$REPO" add -A
git -C "$REPO" commit -qm "good revision"
REV="$(git -C "$REPO" rev-parse HEAD)"

# Dirty the shared tree: an edit to a tracked file and a syntax error that was
# never committed. A proof that reads the tree would export both.
printf 'UNCOMMITTED EDIT\n' >"$REPO/keep.txt"
printf 'this is not rust (((\n' >"$REPO/dirty.rs"

# shellcheck source=proof-lib.sh
. "$ROOT/scripts/proof-lib.sh"
proof_archive "$REPO" "$REV" "$DEST" || fail "proof_archive failed"

[ -f "$DEST/keep.txt" ] || fail "the committed file is missing from the export"
grep -Fq "committed value" "$DEST/keep.txt" || fail "the export took the dirty edit"
[ ! -e "$DEST/dirty.rs" ] || fail "the dirty syntax-error file leaked into the export"
[ -f "$DEST/src/lib.rs" ] || fail "a committed path is missing from the export"
pass "the export carries the revision, not the shared tree"

# The revision the driver archives is the one it names (PROVEN_SHA): the export
# and the receipt can never disagree.
PROVEN="$(grep -o 'git -C "$ROOT" archive "$PROVEN_SHA"' "$ROOT/scripts/clean-proof.sh" | head -1)"
[ -n "$PROVEN" ] || fail "the driver no longer archives PROVEN_SHA"
pass "the driver archives the proven revision, not the tree"

echo "ok"
