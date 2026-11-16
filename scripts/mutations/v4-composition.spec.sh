# Sourced by scripts/verify-mutations.sh.
# bd-6setu.17 (ADR-0022 V4 composition): the plan reports a seat's effort
# honored because the PANE launch carries it. If the headless leaf drops the
# field again, the plan is a lie for every headless run - the join V4 and V4-ext
# never made.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(the_plan_and_the_leaf_agree_on_every_field)

mutation "C1  the headless pi leaf drops the effort the plan reports" \
    file=src/headless.rs \
    expect=the_plan_and_the_leaf_agree_on_every_field \
    find='                argv.extend(model_args("pi", m, effort));' \
    replace='                argv.extend(model_args("pi", m, None));'
