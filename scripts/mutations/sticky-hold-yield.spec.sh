# Sourced by scripts/verify-mutations.sh.
# bd-mww6u: the sticky hold must yield to a live fleet / contention.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    sticky_hold_yields_to_a_live_fleet
    sticky_hold_yields_to_cross_lane_contention
    sticky_hold_stands_without_a_live_fleet
)

mutation "H1  the sticky hold is unconditional (never yields)" \
    file=src/briefing.rs \
    expect=sticky_hold_yields_to_a_live_fleet \
    find='pub fn sticky_hold_evidence(world: &World) -> Option<String> {
    if world.joinable && world.pin_count > 0 {' \
    replace='pub fn sticky_hold_evidence(world: &World) -> Option<String> {
    if false && world.joinable && world.pin_count > 0 {'

mutation "H2  contention never releases the hold" \
    file=src/briefing.rs \
    expect=sticky_hold_yields_to_cross_lane_contention \
    find='    if world.held_paths.len() > 1 {' \
    replace='    if false && world.held_paths.len() > 1 {'
