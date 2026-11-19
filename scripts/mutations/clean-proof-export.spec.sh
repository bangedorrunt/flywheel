# Sourced by scripts/verify-mutations.sh.
# bd-kszuj: clean-proof proves the COMMIT, not the shared tree. proof_archive
# is the step that makes that true; copy the tree instead and the dirty export
# leaks into the proof.

CARGO_ARGS=(--test clean_proof_export)
TEST_FILTER=(the_export_carries_the_commit_not_the_shared_tree)

mutation "E1  the export copies the dirty tree instead of the commit" \
    file=scripts/proof-lib.sh \
    expect=the_export_carries_the_commit_not_the_shared_tree \
    find='  proof_git -C "$repo" archive "$rev" | tar -x -C "$dest"' \
    replace='  cp -R "$repo/." "$dest"'
