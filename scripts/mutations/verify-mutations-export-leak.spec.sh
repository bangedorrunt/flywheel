# Sourced by scripts/verify-mutations.sh.
# bd-3681w: every spec run makes a ~4-5 GB scratch export. An EXIT trap must end
# it on success, RED, error and signal; clean-proof --keep must prune older kept
# exports. 668 leaked exports (127 GB) filled the disk and killed proof44.

CARGO_ARGS=(--test proof_steps)
TEST_FILTER=(proof_verify_mutations_never_edits_the_shared_tree proof_clean_proof_keep_prunes_older_exports)

mutation "E1  no exit trap removes the scratch export" \
    file=scripts/verify-mutations.sh \
    expect=proof_verify_mutations_never_edits_the_shared_tree \
    find='    trap on_exit EXIT' \
    replace='    : # bd-3681w mutant: no export cleanup trap'

mutation "E2  --keep never prunes older kept exports" \
    file=scripts/clean-proof.sh \
    expect=proof_clean_proof_keep_prunes_older_exports \
    find='-name '\''fw-proof.*'\'' ! -path "$TMP"' \
    replace='-name '\''bd-3681w-mutant'\'' ! -path "$TMP"'
