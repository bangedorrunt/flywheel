# Sourced by scripts/verify-mutations.sh. bd-01q9d: consent, dry run and
# session reuse on swarm apply are load-bearing safety gates.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_role_patch_without_yes_names_every_respawn a_dry_run_apply_emits_no_effects_and_advances_no_plan a_respawn_keeps_the_recorded_harness_session seat_adopt_rebinds_the_pane_and_rex_block_verifies a_harness_patch_drops_the_old_model)

mutation "S1  a role respawn no longer needs consent" \
    file=src/cmd/swarm.rs \
    expect=a_role_patch_without_yes_names_every_respawn \
    find='            SeatEffect::Respawn { pin, .. } => Some(pin.clone()),' \
    replace='            SeatEffect::Respawn { .. } => None,'

mutation "S2  dry-run still dispatches effects" \
    file=src/cmd/swarm.rs \
    expect=a_dry_run_apply_emits_no_effects_and_advances_no_plan \
    find='    if dry_run {' \
    replace='    if false {'

mutation "S3  a respawn forgets the recorded session" \
    file=src/cmd/swarm.rs \
    expect=a_respawn_keeps_the_recorded_harness_session \
    find='        args.push("--session".to_string());' \
    replace='        args.push("--new-session".to_string());'

mutation "S4  seat adopt leaves the pane unbound" \
    file=src/cmd/swarm.rs \
    expect=seat_adopt_rebinds_the_pane_and_rex_block_verifies \
    find='    entry.pane_id = pane.to_string();' \
    replace='    entry.pane_id = entry.pane_id.clone();'

mutation "S5  a harness swap keeps the old model" \
    file=src/cmd/swarm.rs \
    expect=a_harness_patch_drops_the_old_model \
    find='                    if !model_patched.contains(&i) {' \
    replace='                    if false {'
