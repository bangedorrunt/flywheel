# Sourced by scripts/verify-mutations.sh.
# bd-6setu.8 (ADR-0022 V4): the plan's status must match what the adapter's own
# argv/env carries. Report honored unconditionally and the property's refused
# half disappears - every seat claims a delivery the launcher never makes.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(every_harness_field_is_honored_iff_the_launch_emits_it)

mutation "V4  every field reports honored whatever the adapter emits" \
    file=src/swarm_plan.rs \
    expect=every_harness_field_is_honored_iff_the_launch_emits_it \
    find='    if delivered {' \
    replace='    if delivered || true {'
