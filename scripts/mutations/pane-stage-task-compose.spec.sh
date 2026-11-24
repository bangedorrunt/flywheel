# Sourced by scripts/verify-mutations.sh.
# Stage A D9: pane_dispatch_scoped must compose stage.prompt + task instead of dropping stage rules.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    compose_dispatch_prompt_preserves_stage_when_task_is_present
)

mutation "P1  compose_dispatch_prompt drops stage_prompt when task is present" \
    file=src/pane.rs \
    expect=compose_dispatch_prompt_preserves_stage_when_task_is_present \
    find='    match (stage, task) {
        (Some(stage), Some(task)) => format!("{stage}\n\nTASK: {task}"),
        (Some(stage), None) => stage.to_string(),
        (None, Some(task)) => task.to_string(),
        (None, None) => fallback.to_string(),
    }' \
    replace='    let _ = stage;
    match task {
        Some(task) => task.to_string(),
        None => fallback.to_string(),
    }'
