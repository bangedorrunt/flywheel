#!/usr/bin/env bash
# bd-j45zo: --keep must prune older fw-proof.* dirs but never the export it just
# kept, even when TMPDIR ends in '/'. A red proof must not prune at all.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE="$(mktemp -d "${TMPDIR:-/tmp}/clean-proof-keep-test.XXXXXX")"
cleanup() { rm -rf "$BASE"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# macOS TMPDIR ends in '/'. mktemp then builds .../T//fw-proof.X while find
# prints .../T/fw-proof.X, so the old ! -path "$TMP" exclusion never matched.
export TMPDIR="$BASE/"
unset CLEAN_PROOF_SLOTTED CLEAN_PROOF_SNAPSHOTTED CLEAN_PROOF_SNAPSHOT_DIR
# clean-proof checks that ROOT is a git repo before reaching the test hook. A
# mutation scratch export has no .git, so point it at a throwaway one.
export CLEAN_PROOF_REPO_ROOT="$BASE/gitrepo"
git init -q "$CLEAN_PROOF_REPO_ROOT"
export CLEAN_PROOF_LOCK_FILE="$BASE/clean-proof.lock"

run_proof() { # $1 = the simulated proof exit status
  set +e
  CLEAN_PROOF_KEEP_TEST="$1" bash "$ROOT/scripts/clean-proof.sh" --keep >/dev/null 2>&1
  local rc=$?
  set -e
  [ "$rc" -eq "$1" ] || fail "clean-proof exited $rc, expected $1"
}

kept_dirs() {
  local d
  for d in "$BASE"/fw-proof.*; do
    [ -d "$d" ] && printf '%s\n' "$d"
  done
}

# --- green proof, trailing-slash TMPDIR, one older kept export ---
OLD="$(mktemp -d "${TMPDIR}fw-proof.old.XXXXXX")"
printf 'old\n' >"$OLD/marker"
run_proof 0
dirs=()
while IFS= read -r d; do dirs+=("$d"); done < <(kept_dirs)
[ "${#dirs[@]}" -eq 1 ] || fail "expected 1 kept export, got ${#dirs[@]}: ${dirs[*]}"
case "${dirs[0]}" in
  *"$(basename "$OLD")"*) fail "the old export survived a green proof" ;;
esac
CURRENT="${dirs[0]}"
pass "trailing-slash TMPDIR: old export pruned, current kept"

# bd-9dxjt: --keep keeps logs/receipt/source but not the multi-GB target dirs.
[ -f "$CURRENT/cache-stores.tsv" ] || fail "kept export lost its non-target artifacts"
[ ! -e "$CURRENT/target" ] || fail "kept export still holds the proof-owned target dir"
[ ! -e "$CURRENT/toron/target" ] || fail "kept export still holds the sibling target dir"
pass "kept export drops target dirs, keeps the rest"

# --- a red proof must leave the previous kept export intact ---
printf 'previous\n' >"$CURRENT/marker"
run_proof 1
dirs=()
while IFS= read -r d; do dirs+=("$d"); done < <(kept_dirs)
[ -d "$CURRENT" ] || fail "a red proof removed the previous kept export"
[ "${#dirs[@]}" -eq 2 ] || fail "expected 2 kept exports after a red proof, got ${#dirs[@]}: ${dirs[*]}"
pass "a red proof leaves the previous kept export intact"

echo "ok"
