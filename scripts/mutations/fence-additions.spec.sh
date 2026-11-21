# Sourced by scripts/verify-mutations.sh.
# bd-8jakq additions: the static sandbox guard and the --modules walk.

CARGO_ARGS=(--test fence_test_export)
TEST_FILTER=(fence_test_export_witness)

mutation "A1  the test-sandbox guard is skipped" \
    file=scripts/fence.sh \
    expect=fence_test_export_witness \
    find='bash "$SANDBOX_CHECK"' \
    replace='true'

mutation "A2  --modules runs no derived modules" \
    file=scripts/fence.sh \
    expect=fence_test_export_witness \
    find='            $CARGO test $args "${module}${m}::" \' \
    replace='            true #'
