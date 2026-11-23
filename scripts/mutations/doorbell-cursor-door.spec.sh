# Sourced by scripts/verify-mutations.sh. bd-slt2n: a plugin's toron cursor
# read must not suppress the wake for an idle events-door pane.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(
    events_door_cursor_read_still_rings_the_idle_pane
    events_pump_unarmed_falls_back_to_a_typed_wake
)

mutation "C1  an events-door pin is treated as already pulled" \
    file=crates/doorbell-core/src/lib.rs \
    expect=events_door_cursor_read_still_rings_the_idle_pane \
    find='                    && !next.events_door_pins.contains(&pin)' \
    replace='                    && true'

mutation "A1  a dead events pump never falls back to a typed wake" \
    file=crates/doorbell-core/src/lib.rs \
    expect=events_pump_unarmed_falls_back_to_a_typed_wake \
    find='                    next.events_door_pins.contains(*pin)' \
    replace='                    false'
