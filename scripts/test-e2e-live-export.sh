#!/usr/bin/env bash
# bd-w5tgg: the live-five leg must compile from a clean export of ONE revision,
# never the shared tree. A syntax error in an unrelated dirty file must not reach
# the export, and the receipt/sha must name the proven revision.
# Hermetic: a throwaway git repo, no rex, no cargo, no live pane.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/e2e-live-export.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT
export TMPDIR="$TMP"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

REPO="$TMP/repo"
mkdir -p "$REPO/src"
(
  cd "$REPO"
  git init -q .
  git config user.email export@localhost
  git config user.name export
  printf 'committed value\n' >src/lib.rs
  git add -A
  git commit -qm good
)
REV="$(git -C "$REPO" rev-parse HEAD)"
# Another lane's WIP in the shared tree: a syntax error in an unrelated file.
printf 'this is not rust ((((\n' >"$REPO/src/lib.rs"
printf 'untracked wip\n' >"$REPO/wip.rs"

out="$(bash "$ROOT/scripts/e2e-live-five.sh" --check-export "$REPO" 2>&1)" \
  || { printf '%s\n' "$out" >&2; fail "--check-export failed"; }
sha="$(printf '%s\n' "$out" | sed -n '1p')"
export_dir="$(printf '%s\n' "$out" | sed -n '2p')"

[ "$sha" = "$REV" ] || fail "export sha $sha != HEAD $REV"
pass "the export names the proven revision"

[ -d "$export_dir/flywheel" ] || fail "no flywheel export at $export_dir"
grep -Fq 'committed value' "$export_dir/flywheel/src/lib.rs" \
  || fail "the export took the dirty edit: $(cat "$export_dir/flywheel/src/lib.rs")"
[ ! -e "$export_dir/flywheel/wip.rs" ] || fail "an untracked WIP file leaked into the export"
pass "the export carries the commit, not the dirty tree"

# The receipt must name that sha, and the tests must run from the export.
src="$(cat "$ROOT/scripts/e2e-live-five.sh")"
printf '%s\n' "$src" | grep -Fq '"sha":"%s"' \
  || fail "the receipt does not name the proven sha"
printf '%s\n' "$src" | grep -Fq 'PROVEN_SHA' \
  || fail "the receipt does not carry the proven sha value"
printf '%s\n' "$src" | grep -Fq 'cd "$TEST_REPO" && "$CARGO_BIN" test' \
  || fail "the live suites do not run from the clean export"
pass "the receipt names the sha and the suites run from the export"

echo "ok"
