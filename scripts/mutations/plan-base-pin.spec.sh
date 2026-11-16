# Sourced by scripts/verify-mutations.sh.
# bd-6setu.15 L1: a plan id pins its base. Let ensure_plan overwrite it and two
# folds under one id disagree with no event explaining the change.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_second_ensure_plan_keeps_the_base)

mutation "L1  a second ensure_plan rewrites the base" \
    file=src/state.rs \
    expect=a_second_ensure_plan_keeps_the_base \
    find='    if record.base.is_none() {' \
    replace='    if true {'
