# Sourced by scripts/verify-mutations.sh.
# bd-6setu.14 (ADR-0022 V4 extension): the headless leaf carries exactly the
# phase args the adapter registry reports. Drop them and the step claims a phase
# the argv never got.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(headless_step_fields_match_the_leaf_argv)

mutation "H1  the leaf drops the phase args the registry reports" \
    file=src/headless.rs \
    expect=headless_step_fields_match_the_leaf_argv \
    find='    let (mut extra, degraded) = phase_launch_args(harness, phase);' \
    replace='    let (mut extra, degraded) = (Vec::new(), phase_launch_args(harness, phase).1);'
