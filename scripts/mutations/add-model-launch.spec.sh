# Sourced by scripts/verify-mutations.sh.
# bd-syne9: `add --model` must reach the launch, not only the registry.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    add_model_reaches_the_launch_argv_for_a_flagged_harness
    add_model_reaches_the_pane_env_for_opencode
    both_add_paths_wire_the_model_into_launch
)

mutation "A1  the model launch helper is inert" \
    file=src/cmd/spawn.rs \
    expect=add_model_reaches_the_launch_argv_for_a_flagged_harness \
    find='    let Some(ask) = ask.filter(|m| !m.trim().is_empty()) else {
        return Ok((Vec::new(), None));
    };
    crate::bare_model_launch(
        kind,
        &crate::models::ModelChoice::parse(ask.trim()),
        "",
        None,
    )' \
    replace='    let _ = ask;
    Ok((Vec::new(), None))'

mutation "A2  the rex add path drops the model launch" \
    file=src/cmd/spawn.rs \
    expect=both_add_paths_wire_the_model_into_launch \
    find='            let (mut add_args, env_model_pin) = add_model_launch(&kind, model)?;' \
    replace='            let (mut add_args, env_model_pin): (Vec<String>, Option<String>) = (Vec::new(), None);'

mutation "A3  the hc add path drops the model launch" \
    file=src/cmd/spawn.rs \
    expect=both_add_paths_wire_the_model_into_launch \
    find='            let (launch_args, env_model_pin) = add_model_launch(&kind, model.as_deref())?;' \
    replace='            let (launch_args, env_model_pin): (Vec<String>, Option<String>) = (Vec::new(), None);'
