# Sourced by scripts/verify-mutations.sh.
# bd-3vkpe: the doorbell's wake targets are the pins whose pane the plane still
# holds — a retired or gone pin's registry row is not a place to ring.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(live_pins_tests)

mutation "L1  every registry row is a wake target" \
    file=src/doorbell_host.rs \
    expect=a_gone_pins_pane_is_not_a_wake_target \
    find='        .filter(|e| live.contains(&e.pane_id))' \
    replace='        .filter(|_e| true)'
