# Sourced by scripts/verify-mutations.sh. bd-himh8: flywheel commit heals stale
# stages across the whole shared index, not only the paths it commits.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(stale_stages_outside_the_committed_paths_are_healed)

mutation "H1  only named stale stages are healed" \
    file=src/cmd/commit.rs \
    expect=stale_stages_outside_the_committed_paths_are_healed \
    find='            sync_stale_stage(&shared, &class.stale, &commit)?;' \
    replace='            sync_stale_stage(&shared, &class.conflict, &commit)?;'
