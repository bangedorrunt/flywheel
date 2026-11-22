# Sourced by scripts/verify-mutations.sh.
# bd-lfii3: clean-proof must run each pinned sibling's own test suite.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    a_failing_sibling_suite_fails_the_step_naming_it
)

mutation "B1  sibling test step is skipped" \
    file=scripts/clean-proof.sh \
    expect=a_failing_sibling_suite_fails_the_step_naming_it \
    find='    run_step "sibling-$sib_name-tests" env "CARGO_TARGET_DIR=$TMP/$sib_name/test-target" \
      "${MBX[@]}" test --manifest-path "$TMP/$sib_name/Cargo.toml" --workspace || true' \
    replace='    : # sibling suite skipped'
