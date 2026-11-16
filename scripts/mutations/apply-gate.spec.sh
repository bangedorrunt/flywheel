# Sourced by scripts/verify-mutations.sh.
# bd-6setu.15 H1/M2: the plan events land only after every gate. Append them
# above the consent gate and a refused apply leaves its patch in the plan of
# record - a change the operator explicitly did not approve.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_refused_apply_leaves_the_plan_file_byte_identical)

mutation "H1  the events are appended before the consent gate" \
    file=src/cmd/swarm.rs \
    expect=a_refused_apply_leaves_the_plan_file_byte_identical \
    find='    let mut pending = record.clone();' \
    replace='    crate::state::append_plan_events(project, &patches.iter().map(|s| format!("patch:{s}")).chain(counts.iter().map(|s| format!("count:{s}"))).collect::<Vec<_>>())?;
    let mut pending = record.clone();'
