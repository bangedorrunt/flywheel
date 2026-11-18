# Sourced by scripts/verify-mutations.sh.
# bd-89q4r: retire cleared the state registry but left the project-local
# mirror, and load_spawn_registry falls back to that mirror once the registry
# is empty - so the gone seat came back as idle and blocked the next spawn.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(retire_clears_the_project_mirror_too)

mutation "R1  retire forgets the project-local mirror" \
    file=src/cmd/swarm.rs \
    expect=retire_clears_the_project_mirror_too \
    find='    crate::state::save_agent_names(&project_dir, &mirror)?;' \
    replace='    let _ = &mirror;'
