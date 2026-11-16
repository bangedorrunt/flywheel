# Sourced by scripts/verify-mutations.sh.
# bd-jhz8x.4 (ADR-0021 D6): an unused stage is a workflow-verify failure.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(an_undeclared_unreferenced_stage_fails_and_a_declaration_clears_it)

mutation "U1  the gate ignores the catalog reference set" \
    file=src/stages.rs \
    expect=an_undeclared_unreferenced_stage_fails_and_a_declaration_clears_it \
    find='    unreferenced_stages(all, referenced)' \
    replace='    all.to_vec()'
