#!/usr/bin/env bash
# The crew pre-mail check (bd-koafr follow-up): prove a COMMIT with the proof's
# fail-fast prefix, in a clean export, so a dirty tree cannot change the answer.
#
#   scripts/fence.sh [--sha <rev>] [--repo <dir>] [--keep]
#   scripts/fence.sh --index        # the STAGED index, as the pre-commit hook
#   scripts/fence.sh --paths a.rs,b.rs --test cmd::spawn::   # bd-8jakq
#
# `--test <filter>` runs that test filter INSIDE the throwaway export (after
# the fail-fast build), so another lane's uncommitted WIP in the shared tree
# cannot break your targeted tests. `--paths` overlays a not-yet-staged set of
# working-tree paths on HEAD for that export; without it the export is the
# named revision. `--modules` derives the test modules of each changed .rs file
# and runs every one of them in the export (bd-8jakq comments: proof86 died on
# four tests a single added filter missed). The static test-sandbox guard runs
# before the build (proof89). The export is removed on exit; a kept one is
# swept on the next green run so old exports do not pile up.
#
# Steps, in the proof's order, stopping at the first failure:
#   verify-needles · verify-coverage · build -Dwarnings --all-targets ·
#   test --no-run · verify-clippy
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
TEST_FILTER=""
PATHS=""
MODULES=0
PIN="${FLYWHEEL_FENCE_PIN:-${AGENT_NAME:-crew}}"
SWEEP_AGE_SECS="${FENCE_SWEEP_AGE_SECS:-14400}" # 4h: another crew's export is never young
CARGO="${FENCE_CARGO:-mbx +nightly-aarch64-apple-darwin}"
RUSTFMT="${FENCE_RUSTFMT:-rustup run nightly-aarch64-apple-darwin rustfmt --edition 2024}"

while [ $# -gt 0 ]; do
    case "$1" in
        --sha) REV="$2"; shift ;;
        --repo) REPO="$2"; shift ;;
        --index) INDEX=1 ;;
        --keep) KEEP=1 ;;
        --test) TEST_FILTER="$2"; shift ;;
        --paths) PATHS="$2"; shift ;;
        --modules) MODULES=1 ;;
        -h | --help) sed -n '2,26p' "$0"; exit 0 ;;
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
TMP="$(mktemp -d "${TMPDIR:-/tmp}/fence.${PIN}.XXXXXX")"
# bd-250g4: a live lock marks a running fence. Never sweep a dir that holds one.
LOCK="$TMP/.fence-running.lock"
: >"$LOCK"

export_is_live() {
    local dir="$1"
    [ -f "$dir/.fence-running.lock" ] && return 0
    if command -v lsof >/dev/null 2>&1 \
        && lsof +D "$dir" >/dev/null 2>&1; then
        return 0
    fi
    return 1
}

sweep_kept_exports() {
    local dir age now
    now="$(date +%s)"
    # Only OUR PIN's older exports, or ANY export older than a few hours.
    # bd-8jakq's sweep-all deleted concurrent crews' kept exports mid-test.
    for dir in "${TMPDIR:-/tmp}"/fence.*; do
        [ -d "$dir" ] || continue
        [ "$dir" = "$TMP" ] && continue
        case "$(basename "$dir")" in
            fence."$PIN".*) ;;
            *)
                age=$((now - $(stat -f %m "$dir" 2>/dev/null || echo 0)))
                [ "$age" -ge "$SWEEP_AGE_SECS" ] || continue
                ;;
        esac
        export_is_live "$dir" && continue
        rm -rf "$dir"
    done
}

cleanup() {
    local status="${FENCE_TEST_STATUS:-$?}"
    rm -f "$LOCK"
    if [ "$KEEP" -eq 1 ] || [ "$status" -ne 0 ]; then
        echo "fence: kept $TMP" >&2
        [ "$status" -eq 0 ] && sweep_kept_exports
    else
        rm -rf "$TMP"
    fi
}
trap cleanup EXIT
# bd-250g4: test hook — drive the sweep policy without a real build.
if [ -n "${FENCE_SWEEP_TEST:-}" ]; then
    FENCE_TEST_STATUS="${FENCE_SWEEP_TEST_STATUS:-0}" cleanup
    exit "${FENCE_SWEEP_TEST_STATUS:-0}"
fi

# shellcheck source=proof-lib.sh
. "$ROOT/scripts/proof-lib.sh"
DEST="$TMP/flywheel"
proof_archive "$REPO" "$SHA" "$DEST" || { echo "fence: cannot export $SHA" >&2; exit 1; }
# bd-8jakq: an unstaged path set is overlaid on the revision's export, so a
# red shared tree cannot change what the targeted tests see.
if [ -n "$PATHS" ]; then
    IFS=',' read -r -a overlay <<<"$PATHS"
    for rel in "${overlay[@]}"; do
        [ -n "$rel" ] || continue
        [ -f "$REPO/$rel" ] || { echo "fence: overlay path missing: $rel" >&2; exit 2; }
        mkdir -p "$DEST/$(dirname "$rel")"
        cp "$REPO/$rel" "$DEST/$rel"
    done
fi
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
# bd-8jakq: static and cheap; proof89 died on an unsandboxed spawn the fence
# passed. This guard reads the shared tree's tests, so run it before cd DEST.
step "test sandbox guard"
SANDBOX_CHECK="${FENCE_SANDBOX_CHECK:-$ROOT/scripts/check-test-sandbox.sh}"
bash "$SANDBOX_CHECK"

run_test_filter() {
    [ -n "$TEST_FILTER" ] || return 0
    step "test ($TEST_FILTER) in export"
    # shellcheck disable=SC2086
    $CARGO test --tests $TEST_FILTER
}

# bd-8jakq: every `#[cfg(test)] mod X` in a changed file is a test module of
# that file. Run each in the export so a green added filter cannot hide a red
# sibling module (proof86). Pure derivation from the path + source text.
run_changed_modules() {
    [ "$MODULES" -eq 1 ] || return 0
    local files=()
    # bash 3.2 has no mapfile (ScarletCove hit this on /bin/bash); while-read
    # keeps the same NUL-safe-enough newline list portable.
    if [ "$INDEX" = 1 ]; then
        while IFS= read -r line; do
            [ -n "$line" ] && files+=("$line")
        done < <(git -C "$REPO" diff --cached --name-only --diff-filter=ACM -- '*.rs')
    elif [ -n "$PATHS" ]; then
        IFS=',' read -r -a files <<<"$PATHS"
    else
        while IFS= read -r line; do
            [ -n "$line" ] && files+=("$line")
        done < <(git -C "$REPO" diff --name-only "$SHA" -- '*.rs')
    fi
    local f crate args module mods=""
    for f in "${files[@]}"; do
        [ -f "$f" ] || continue
        case "$f" in
            crates/*/src/lib.rs)
                crate="$(printf '%s' "$f" | cut -d/ -f2)"
                args="-p $crate --lib"
                module=""
                ;;
            crates/*/src/*.rs)
                crate="$(printf '%s' "$f" | cut -d/ -f2)"
                args="-p $crate --lib"
                module="$(basename "$f" .rs)::"
                ;;
            src/main.rs)
                args="-p flywheel --bin flywheel"
                module=""
                ;;
            src/cmd/*.rs)
                args="-p flywheel --bin flywheel"
                module="cmd::$(basename "$f" .rs)::"
                ;;
            src/*.rs)
                args="-p flywheel --bin flywheel"
                module="$(basename "$f" .rs)::"
                ;;
            *) continue ;;
        esac
        mods=()
        while IFS= read -r line; do
            [ -n "$line" ] && mods+=("$line")
        done < <(grep -A1 '#\[cfg(test)\]' "$f" | sed -n 's/^[[:space:]]*mod[[:space:]]\([A-Za-z0-9_]*\)[[:space:]]*{.*/\1/p' | sort -u)
        for m in "${mods[@]}"; do
            step "module ($f :: $m) in export"
            # shellcheck disable=SC2086
            $CARGO test $args "${module}${m}::" \
                || { echo "fence: module $f::$m failed" >&2; return 1; }
        done
    done
}
step "needles"
bash scripts/verify-needles.sh
# bd-op4zk: a ledger row citing a mutation id the spec no longer declares used
# to die 14 minutes into a proof. Coverage is sub-second; run it at commit time.
step "coverage ledger"
bash scripts/verify-coverage.sh
if [ "$INDEX" = 1 ]; then
    # bd-4z7mb: the commit gate. Clippy runs the same type check as `cargo
    # check` plus the lints, so a clippy-only regression cannot slip past it
    # (f93a8f42 did exactly that under the old check leg).
    step "clippy (-D warnings, all targets)"
    # shellcheck disable=SC2086
    $CARGO clippy --workspace --all-targets -- -D warnings
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
    run_changed_modules
    run_test_filter
    echo "fence: $SHA GREEN (index)"
    exit 0
fi
step "test --no-run (every target)"
# shellcheck disable=SC2086
$CARGO test --workspace --no-run
step "clippy"
bash scripts/verify-clippy.sh
run_changed_modules
run_test_filter
echo "fence: $SHA GREEN"
