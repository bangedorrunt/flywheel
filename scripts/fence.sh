#!/usr/bin/env bash
# The crew pre-mail check (bd-koafr follow-up): prove a COMMIT with the proof's
# fail-fast prefix, in a clean export, so a dirty tree cannot change the answer.
#
#   scripts/fence.sh [--sha <rev>] [--repo <dir>] [--keep]
#   scripts/fence.sh --index        # the STAGED index, as the pre-commit hook
#
# Steps, in the proof's order, stopping at the first failure:
#   verify-needles · build -Dwarnings --all-targets · test --no-run ·
#   verify-clippy
# `--index` (bd-4z7mb) fences exactly what a commit would land: `git write-tree`
# over the staging area, a throwaway commit off HEAD, then needles, the
# -Dwarnings build and `rustfmt --check` on the staged .rs files. It is the
# commit-time gate: a caller staged without its callee builds in the dirty tree
# and dies in this export.
# Export comes from scripts/proof-lib.sh (proof_archive), the same function
# clean-proof uses — the two doors cannot disagree about what a commit is.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="$ROOT"
REV="HEAD"
KEEP=0
INDEX=0
CARGO="${FENCE_CARGO:-mbx +nightly-aarch64-apple-darwin}"
RUSTFMT="${FENCE_RUSTFMT:-rustup run nightly-aarch64-apple-darwin rustfmt --edition 2024}"

while [ $# -gt 0 ]; do
    case "$1" in
        --sha) REV="$2"; shift ;;
        --repo) REPO="$2"; shift ;;
        --index) INDEX=1 ;;
        --keep) KEEP=1 ;;
        -h | --help) sed -n '2,22p' "$0"; exit 0 ;;
        *) echo "fence: unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

STAGED_RS=""
if [ "$INDEX" = 1 ]; then
    # bd-4z7mb: fence what the commit WOULD land. A throwaway commit off HEAD
    # carries the staged tree; git archive then exports exactly that.
    TREE="$(git -C "$REPO" write-tree)"
    SHA="$(printf '%s\n' 'fence-index' | git -C "$REPO" \
        -c user.name='fence' -c user.email='fence@localhost' \
        commit-tree "$TREE" -p HEAD)"
    STAGED_RS="$(git -C "$REPO" diff --cached --name-only --diff-filter=ACM -- '*.rs')"
else
    SHA="$(git -C "$REPO" rev-parse "$REV")"
fi
TMP="$(mktemp -d "${TMPDIR:-/tmp}/fence.XXXXXX")"
cleanup() { [ "$KEEP" -eq 1 ] || rm -rf "$TMP"; }
trap cleanup EXIT

# shellcheck source=proof-lib.sh
. "$ROOT/scripts/proof-lib.sh"
DEST="$TMP/flywheel"
proof_archive "$REPO" "$SHA" "$DEST" || { echo "fence: cannot export $SHA" >&2; exit 1; }
# The fence proves the COMMIT, not the tree, including the pins: the siblings
# come from the fenced revision's proof.toml (bd-gmrxv), never from the working
# tree, so a shared tree's dirty pin cannot change what "GREEN at <sha>" covers.
while IFS=$'\t' read -r rel pin; do
    [ -n "${rel:-}" ] || continue
    sib_repo="$(cd "$REPO/$rel" && pwd)"
    proof_archive "$sib_repo" "$pin" "$TMP/$(basename "$rel")" \
        || { echo "fence: cannot archive sibling $rel at $pin" >&2; exit 1; }
done < <(proof_siblings "$REPO" "$SHA")
echo "fence: $SHA exported to $DEST"
cd "$DEST"

step() { printf 'fence: %s\n' "$1"; }
step "needles"
bash scripts/verify-needles.sh
if [ "$INDEX" = 1 ]; then
    # bd-4z7mb: the commit gate wants name resolution, not codegen. `check` is
    # the same caller/callee detector the split commits needed, at a fraction of
    # the wall time, so a hook stays usable.
    step "check (-D warnings, all targets)"
    # shellcheck disable=SC2086
    RUSTFLAGS=-Dwarnings $CARGO check --workspace --all-targets
else
    step "build (-D warnings, all targets)"
    # shellcheck disable=SC2086
    RUSTFLAGS=-Dwarnings $CARGO build --workspace --all-targets
fi
if [ "$INDEX" = 1 ]; then
    # bd-4z7mb: the commit-time gate is the fast prefix, and its fmt leg reads
    # the STAGED content (the export), not the files still dirty in the tree.
    [ -n "$STAGED_RS" ] || { echo "fence: nothing staged to format"; }
    for f in $STAGED_RS; do
        [ -f "$f" ] || continue
        step "fmt ($f)"
        # shellcheck disable=SC2086
        $RUSTFMT --check "$f"
    done
    echo "fence: $SHA GREEN (index)"
    exit 0
fi
step "test --no-run (every target)"
# shellcheck disable=SC2086
$CARGO test --workspace --no-run
step "clippy"
bash scripts/verify-clippy.sh
echo "fence: $SHA GREEN"
