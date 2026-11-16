# Sourced by scripts/verify-mutations.sh.
# bd-4z7mb follow-up: --dry-run must create nothing. Let the dry arm fall through
# to the real run and it creates a session the operator did not ask for.

CARGO_ARGS=(--test e2e_live_dry_run)
TEST_FILTER=(the_live_five_dry_run_plans_without_creating_a_session)

mutation "D1  the dry run creates the session it only describes" \
    file=scripts/e2e-live-five.sh \
    expect=the_live_five_dry_run_plans_without_creating_a_session \
    find='    exit 0
fi'
    replace='    "$REX_BIN" new -s "$SESSION" --name "$PIN" --block "$BLOCK" >/dev/null 2>&1 || true
    exit 0
fi'
