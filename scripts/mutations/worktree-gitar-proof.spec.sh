# Sourced by scripts/verify-mutations.sh.
# bd-1umbz: cross-repo proof_archive must not inherit a worktree GIT_DIR.

CARGO_ARGS=(--test fence_pinned_siblings)
TEST_FILTER=(the_fence_reads_the_fenced_revisions_pins)

mutation "G1  proof_archive inherits the worktree GIT_DIR" \
    file=scripts/proof-lib.sh \
    expect=the_fence_reads_the_fenced_revisions_pins \
    find='  proof_git -C "$repo" cat-file -e "${rev}^{commit}" 2>/dev/null \' \
    replace='  git -C "$repo" cat-file -e "${rev}^{commit}" 2>/dev/null \'
