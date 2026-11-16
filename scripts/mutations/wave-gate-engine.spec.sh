# Sourced by scripts/verify-mutations.sh.
# ADR-0021 D4.5 (bd-jhz8x.2): the wave gate is an ENGINE step. A lower open wave
# must hold (naming blockers); an empty ready with this wave in flight is work
# in flight; an empty ready with nothing in flight is a loud failure.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(cmd::wave_gate)

mutation "G1  a lower open wave no longer holds" \
    file=src/cmd/wave_gate.rs \
    expect=a_lower_open_wave_holds_and_names_the_blockers \
    find='        .filter(|b| bead_wave(b).is_some_and(|w| w < n))' \
    replace='        .filter(|_b| false)'

mutation "G2  an empty ready with work in flight reads as a failure" \
    file=src/cmd/wave_gate.rs \
    expect=an_empty_ready_with_this_wave_in_flight_is_work_in_flight \
    find='    if rows.is_empty() && in_flight.is_empty() {' \
    replace='    if rows.is_empty() {'
