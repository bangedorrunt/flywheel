# Sourced by scripts/verify-mutations.sh.
# Arrive must bind to the rex block when set, not fall back to the plane pane
# (bd-ksbtp / inbox-ports.arrive-pane).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(inbox_ports::tests::arrive_pane_prefers_rex_block_and_falls_back_when_it_is_empty)

mutation "A1  arrive ignores rex block and uses plane pane" \
    file=src/inbox_ports.rs \
    expect=inbox_ports::tests::arrive_pane_prefers_rex_block_and_falls_back_when_it_is_empty \
    find=$'    let rex = rex_block.map(str::trim).filter(|s| !s.is_empty());\n    if rex.is_some() {\n        return rex;\n    }\n    plane_pane.map(str::trim).filter(|s| !s.is_empty())' \
    replace=$'    let _rex = rex_block.map(str::trim).filter(|s| !s.is_empty());\n    plane_pane.map(str::trim).filter(|s| !s.is_empty())'
