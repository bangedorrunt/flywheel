# Sourced by scripts/verify-mutations.sh.
# bd-oo7zi: the live e2e pin must be registered in the toron project BEFORE the
# wake. Without it, every wake fails 'unknown recipient: LiveFivePin' instead of
# testing the pane guard.

CARGO_ARGS=(--test live_doorbell_operator_guard)
TEST_FILTER=(a_wake_registers_its_pin_first)

mutation "R1  the live wake skips pin registration" \
    file=tests/live_doorbell_operator_guard.rs \
    expect=a_wake_registers_its_pin_first \
    find='fn bootstrap_pin(pin: &str, project: &str) {' \
    replace='fn bootstrap_pin(pin: &str, project: &str) {
    return; // mutation: no bootstrap, so the wake addresses an unknown pin'
