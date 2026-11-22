# Sourced by scripts/verify-mutations.sh.
# bd-ihiqo.4: old stage names resolve to the 8 canonical ce- stages.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    old_stage_names_resolve_as_aliases_for_one_release
)

mutation "A1  the stage alias table is ignored" \
    file=src/stages.rs \
    expect=old_stage_names_resolve_as_aliases_for_one_release \
    find='        STAGE_ALIASES
            .iter()
            .find(|(old, _)| *old == name)' \
    replace='        Vec::<(&str, &str)>::new()
            .iter()
            .find(|(old, _)| *old == name)'
