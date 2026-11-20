# Sourced by scripts/verify-mutations.sh.
# bd-uawqo: the converter's double-ring mapping must be what makes the doctored
# trace non-conforming. Break it and the non-conformance witness goes RED.
# Run with FLYWHEEL_EVERYSTATE exported so the witness runs everystate conform.

CARGO_ARGS=(--test conformance_deferred_queue)
TEST_FILTER=(a_recorded_deferral_restart_and_boot_drain_conforms)

mutation "R1  the doctored recorder never emits the second Ring" \
    file=crates/doorbell-rt/src/effect_trace.rs \
    expect=a_recorded_deferral_restart_and_boot_drain_conforms \
    find='                if self.ring_twice {' \
    replace='                if false {'
