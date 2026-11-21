# Sourced by scripts/verify-mutations.sh.
# bd-rrlo2: spawn mints a per-pin events token before launch — skipping the
# mint (or the rotation) must leave the seat tokenless, which the spawn
# test detects.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(only_events_door_harnesses_get_a_token spawn_mints_a_per_pin_token_before_launch respawning_one_seat_never_revokes_another_seat_token)

mutation "T1  no harness gets an events token" \
    file=src/cmd/spawn.rs \
    expect=only_events_door_harnesses_get_a_token \
    find='    || matches!(canonical.as_str(), "opencode" | "pi" | "omp")' \
    replace='    || matches!(canonical.as_str(), "no-such-harness")'

mutation "T2  an expired token is never rotated" \
    file=src/cmd/spawn.rs \
    expect=spawn_mints_a_per_pin_token_before_launch \
    find='            return Ok(SeatTokenOutcome::Rotated);' \
    replace='            return Ok(SeatTokenOutcome::Current);'

mutation "T3  the journal lookup ignores the pin" \
    file=src/events_tokens.rs \
    expect=respawning_one_seat_never_revokes_another_seat_token \
    find='        if row.get("pin").and_then(Value::as_str) != Some(agent)
            || row.get("project").and_then(Value::as_str) != Some(project)' \
    replace='        if row.get("project").and_then(Value::as_str) != Some(project)'
