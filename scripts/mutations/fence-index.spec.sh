# Sourced by scripts/verify-mutations.sh.
# bd-4z7mb: the commit gate proves the STAGED index. Export the working tree
# instead and a staged caller whose callee is only dirty builds here while the
# commit it would land breaks HEAD for every lane.

CARGO_ARGS=(--test fence_staged_index)
TEST_FILTER=(the_fence_refuses_a_staged_caller_without_its_callee)

mutation "I1  the gate exports the working tree, not the staged index" \
    file=scripts/fence.sh \
    expect=the_fence_refuses_a_staged_caller_without_its_callee \
    find='proof_archive "$REPO" "$SHA" "$DEST" || { echo "fence: cannot export $SHA" >&2; exit 1; }' \
    replace='cp -R "$REPO/." "$DEST"'
