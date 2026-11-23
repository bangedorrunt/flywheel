# Sourced by scripts/verify-mutations.sh. bd-slt2n: a plugin's toron cursor
# read must not suppress the wake for an idle events-door pane.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(
    events_door_cursor_read_still_rings_the_idle_pane
    events_pump_unarmed_falls_back_to_a_typed_wake
    events_door_cursor_read_carries_an_explicit_read_command
)

mutation "C1  an events-door pin is treated as already pulled" \
    file=crates/doorbell-core/src/lib.rs \
    expect=events_door_cursor_read_still_rings_the_idle_pane \
    find='                    && !next.events_door_pins.contains(&pin)' \
    replace='                    && true'

mutation "R1  a consumed cursor still says toron next" \
    file=crates/doorbell-core/src/lib.rs \
    expect=events_door_cursor_read_carries_an_explicit_read_command \
    find='                        &mail_since_hint(created_us.expect("cursor_past implies a time")),' \
    replace='                        &(hint.to_string()),'

mutation "A1  a dead events pump never falls back to a typed wake" \
    file=crates/doorbell-core/src/lib.rs \
    expect=events_pump_unarmed_falls_back_to_a_typed_wake \
    find='                    next.events_door_pins.contains(*pin)' \
    replace='                    false'
