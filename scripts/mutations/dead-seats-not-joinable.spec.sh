# Sourced by scripts/verify-mutations.sh.
# bd-s109e: dead crew panes must not make a fleet look joinable.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    dead_registered_seats_are_not_a_joinable_fleet
    lone_plane_pane_is_not_joinable
)

mutation "D1  liveness check skipped" \
    file=src/snapshot_reporting.rs \
    expect=dead_registered_seats_are_not_a_joinable_fleet \
    find='    pins.iter()
        .any(|pin| !looks_like_pane_id(&pin.pin) && !pin.pin.is_empty() && pin.status != "dead")' \
    replace='    pins.iter().any(|pin| !looks_like_pane_id(&pin.pin) && !pin.pin.is_empty())'
