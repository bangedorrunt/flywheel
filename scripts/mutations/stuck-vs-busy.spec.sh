# Sourced by scripts/verify-mutations.sh.
# Both-yes is a contradiction and must not collapse to a verdict (bd-3uqz).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(stuck_vs_busy::tests::both_yes_is_a_contradiction_and_is_not_resolved_by_picking_a_side)

mutation "B1  contradiction branch is skipped" \
    file=src/stuck_vs_busy.rs \
    expect=both_yes_is_a_contradiction_and_is_not_resolved_by_picking_a_side \
    find='    if w > YES && k > YES {' \
    replace='    if false && w > YES && k > YES {'
