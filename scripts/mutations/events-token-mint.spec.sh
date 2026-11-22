# Sourced by scripts/verify-mutations.sh.
# bd-rrlo2: spawn mints a per-pin events token before launch — skipping the
# mint (or the rotation) must leave the seat tokenless, which the spawn
# test detects.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(only_events_door_harnesses_get_a_token spawn_mints_a_per_pin_token_before_launch respawning_one_seat_never_revokes_another_seat_token spawn_loop_provisions_a_token_through_the_seam rex_preparation_mints_0600_token_and_points_seat_config both_spawn_paths_call_events_preparation)

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

mutation "T4  the spawn step never reaches the seam" \
    file=src/cmd/spawn.rs \
    expect=spawn_loop_provisions_a_token_through_the_seam \
    find='    let Some(pin) = pin else { return };
    provision_seat_events_token(cli, state_dir, project, kind, pin, now_us);' \
    replace='    let _ = (cli, state_dir, project, kind, pin, now_us);'

mutation "T5  the rex path never prepares its seats" \
    file=src/cmd/spawn.rs \
    expect=both_spawn_paths_call_events_preparation \
    find='                prepare_seat_events_before_launch(
                    &cli,
                    &crate::config::state_dir(),
                    &kind,
                    project,
                    mail_identity.as_deref(),
                    &mut pane_env,
                );
            }
            let cwd = utf8_path(&dir, "workspace dir")?;' \
    replace='                let _ = (&cli, &mut pane_env);
            }
            let cwd = utf8_path(&dir, "workspace dir")?;'
