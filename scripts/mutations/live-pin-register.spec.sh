# Sourced by scripts/verify-mutations.sh.
# bd-oo7zi: the live e2e default pin must be a real two-word pin (toron's own
# validator) AND the wake must register it before typing. The first witness
# used a fake toron that never ran the validator, so a three-word default
# passed here and failed live.

CARGO_ARGS=(--test live_doorbell_operator_guard)
TEST_FILTER=(a_wake_registers_a_valid_pin_first)

mutation "R1  the live wake skips pin registration" \
    file=tests/live_doorbell_operator_guard.rs \
    expect=a_wake_registers_a_valid_pin_first \
    find='fn bootstrap_pin(pin: &str, project: &str) {' \
    replace='fn bootstrap_pin(pin: &str, project: &str) {
    return; // mutation: no bootstrap, so the wake addresses an unknown pin'

mutation "P1  the live e2e default pin is not a real pin" \
    file=scripts/e2e-live-five.sh \
    expect=a_wake_registers_a_valid_pin_first \
    find='FLYWHEEL_LIVE_PIN:-AmberProbe' \
    replace='FLYWHEEL_LIVE_PIN:-LiveFivePin'
