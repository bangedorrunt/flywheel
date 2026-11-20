# Sourced by scripts/verify-mutations.sh.
# bd-w5tgg: the live five must build from a clean export, not the shared tree.

CARGO_ARGS=(-p flywheel --test e2e_live_dry_run)
TEST_FILTER=(the_live_five_runs_from_a_clean_export)

mutation "E1  the live suites run from the dirty shared tree again" \
    file=scripts/e2e-live-five.sh \
    expect=the_live_five_runs_from_a_clean_export \
    find='    if (cd "$TEST_REPO" && "$CARGO_BIN" test --test "$(basename "$t" .rs)" -- --ignored --nocapture >"$log" 2>&1); then' \
    replace='    if ("$CARGO_BIN" test --test "$(basename "$t" .rs)" -- --ignored --nocapture >"$log" 2>&1); then'
