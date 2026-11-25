# Sourced by scripts/verify-mutations.sh.
# bd-gcoq1: a resumed seat re-binds by harness session; each target pane is its own journal key.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    seat_adopt_by_harness_session_moves_a_resumed_seat
    seat_adopt_key_is_per_target_pane
)

mutation "S1  session lookup never matches" \
    file=src/cmd/swarm.rs \
    expect=seat_adopt_by_harness_session_moves_a_resumed_seat \
    find='.find(|e| e.harness_session.as_deref() == Some(id))' \
    replace='.find(|e| e.harness_session.as_deref() == Some("never"))'

mutation "S2  journal key drops the pane" \
    file=src/cmd/swarm.rs \
    expect=seat_adopt_key_is_per_target_pane \
    find='format!("seat-adopt:{pin}:{pane}")' \
    replace='format!("seat-adopt:{pin}")'
