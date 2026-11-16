# Sourced by scripts/verify-mutations.sh.
# bd-3vkpe: a ring for a pin with no pane resolves as "no pane, nothing to
# ring" instead of ageing into a permanent unanswered conflict.

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(hear_tests)

mutation "A1  a gone pin keeps its ring forever" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=a_gone_pins_ring_is_resolved_not_unanswered \
    find='            .filter(|pin| !live.contains(pin))' \
    replace='            .filter(|_pin| true)'
