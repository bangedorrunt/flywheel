# Sourced by scripts/verify-mutations.sh.
# A bound block without a spawn row still maps to its pin (bd-346jl).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(doorbell_host::bd_346jl_binding_tests::a_bound_block_without_a_spawn_row_still_maps_to_its_pin)

mutation "H1  unmapped bindings are never bridged" \
    file=src/doorbell_host.rs \
    expect=a_bound_block_without_a_spawn_row_still_maps_to_its_pin \
    find=$'        pane_ids\n            .iter()\n            .filter(|pane| !map.contains_key(*pane))\n            .filter_map(|pane| resolve(pane).map(|pin| (pane.clone(), pin)))\n            .collect()' \
    replace='        Vec::new()'
