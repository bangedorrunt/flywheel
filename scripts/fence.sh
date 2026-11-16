#!/usr/bin/env bash
# The crew pre-mail check (bd-koafr follow-up): prove a COMMIT with the proof's
# fail-fast prefix, in a clean export, so a dirty tree cannot change the answer.
#
#   scripts/fence.sh [--sha <rev>] [--repo <dir>] [--keep]
#
# Steps, in the proof's order, stopping at the first failure:
#   verify-needles · build -Dwarnings --all-targets · test --no-run ·
#   verify-clippy
# Export comes from scripts/proof-lib.sh (proof_archive), the same function
# clean-proof uses — the two doors cannot disagree about what a commit is.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="$ROOT"
REV="HEAD"
KEEP=0
CARGO="${FENCE_CARGO:-mbx +nightly-aarch64-apple-darwin}"

while [ $# -gt 0 ]; do
    case "$1" in
        --sha) REV="$2"; shift ;;
        --repo) REPO="$2"; shift ;;
        --keep) KEEP=1 ;;
        -h | --help) sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "fence: unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

SHA="$(git -C "$REPO" rev-parse "$REV")"
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
step "build (-D warnings, all targets)"
# shellcheck disable=SC2086
RUSTFLAGS=-Dwarnings $CARGO build --workspace --all-targets
step "test --no-run (every target)"
# shellcheck disable=SC2086
$CARGO test --workspace --no-run
step "clippy"
bash scripts/verify-clippy.sh
echo "fence: $SHA GREEN"
