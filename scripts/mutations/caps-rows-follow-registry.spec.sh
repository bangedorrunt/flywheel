# Sourced by scripts/verify-mutations.sh.
# bd-6setu.1 (ADR-0022 D3): capability rows enumerate the REGISTRY, so a
# harness added to the registry is capability-known with no second list, and an
# alias answers because the registry says it does.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(capability_rows_are_the_registry)

mutation "R1  the capability rows stop following the registry" \
    file=src/harness_caps.rs \
    expect=capability_rows_are_the_registry \
    find='    crate::harness_registry::REGISTRY
        .iter()
        .map(|s| hc_pane_caps(s.canonical))
        .collect()' \
    replace='    vec![hc_pane_caps("opencode"), hc_pane_caps("pi")]'
