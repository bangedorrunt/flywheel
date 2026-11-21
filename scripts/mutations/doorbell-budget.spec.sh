# Sourced by scripts/verify-mutations.sh.
# bd-l5vb2 verify row: the 4th prompt in a 30-minute window is digested, not
# typed. The single gate is src/doorbell.rs take_allowed -> PinBudget::allows.
#
#   scripts/verify-mutations.sh scripts/mutations/doorbell-budget.spec.sh --cargo mbx

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(the_fourth_prompt_in_thirty_minutes_is_digested)

DOORBELL=src/doorbell.rs

mutation "B1  the per-pin prompt budget is never enforced" \
    file=$DOORBELL \
    expect=the_fourth_prompt_in_thirty_minutes_is_digested \
    find='        if b.allows(now_unix) {' \
    replace='        if true {'
