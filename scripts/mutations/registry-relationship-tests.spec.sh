# Sourced by scripts/verify-mutations.sh.
# bd-6setu.1 (ADR-0022 D3): three tables are merged; two name a DIFFERENT
# domain and are pinned by relationship instead. H1 breaks the hc pin (a row
# naming an integration hc does not have); H2 breaks the no-duplicate-truth pin.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(every_hc_pane_row_names_a_real_integration compiled_in_recipe_harnesses_are_not_registry_rows)

mutation "H1  a row names an hc integration the multiplexer lacks" \
    file=src/harness_registry.rs \
    expect=every_hc_pane_row_names_a_real_integration \
    find='        hc_target: Some("grok"),' \
    replace='        hc_target: Some("grok-tui"),'

mutation "H2  a compiled-in recipe becomes a registry row too" \
    file=src/harness_registry.rs \
    expect=compiled_in_recipe_harnesses_are_not_registry_rows \
    find='        canonical: "fx",
        aliases: &[],' \
    replace='        canonical: "fx",
        aliases: &["pa"],'
