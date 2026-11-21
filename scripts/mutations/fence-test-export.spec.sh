# Sourced by scripts/verify-mutations.sh.
# bd-8jakq: targeted tests must run in the clean export, never the red tree.

CARGO_ARGS=(--bin flywheel)
# The fence test is a shell script, not a Rust test; the Rust test below is the
# witness that the script's own mutation is exercised by scripts/test-fence.sh.
TEST_FILTER=(fence_test_export_witness)

mutation "F1  targeted test runs in the shared tree, not the export" \
    file=scripts/fence.sh \
    expect=fence_test_export_witness \
    find='    $CARGO test --tests $TEST_FILTER' \
    replace='    (cd "$REPO" && $CARGO test --tests $TEST_FILTER)'
