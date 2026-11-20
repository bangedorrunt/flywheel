# Sourced by scripts/verify-mutations.sh.
# bd-3681w: every spec run makes a ~4-5 GB scratch export. An EXIT trap must end
# it on success, RED, error and signal; clean-proof --keep must prune older kept
# exports. 668 leaked exports (127 GB) filled the disk and killed proof44.
# bd-j45zo: the kept-export exclusion must be by basename; the old path-string
# form deleted proof46's own export when TMPDIR ended in '/'.

CARGO_ARGS=(--test proof_steps)
TEST_FILTER=(proof_verify_mutations_never_edits_the_shared_tree proof_clean_proof_keep_survives_trailing_slash_tmpdir)

mutation "E1  no exit trap removes the scratch export" \
    file=scripts/verify-mutations.sh \
    expect=proof_verify_mutations_never_edits_the_shared_tree \
    find='    rm -rf "$OWNED_EXPORT_ROOT"' \
    replace='    : # bd-r7852 mutant: no scratch export cleanup'

mutation "E2  exclude the kept export by its path string again" \
    file=scripts/clean-proof.sh \
    expect=proof_clean_proof_keep_survives_trailing_slash_tmpdir \
    find='! -name "$(basename "$TMP")"' \
    replace='! -path "$TMP"'
