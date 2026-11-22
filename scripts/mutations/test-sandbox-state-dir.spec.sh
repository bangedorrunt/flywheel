# Sourced by scripts/verify-mutations.sh.
# bd-ihiqo.1: a chain test must not write project logs to the real state dir.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    ship_routes_through_the_gated_chain_goal_direct_stays_plain
)

mutation "T1  the chain test runs without a sandboxed env" \
    file=src/phase_chain.rs \
    expect=ship_routes_through_the_gated_chain_goal_direct_stays_plain \
    find='        let scope = sandbox.scope();' \
    replace='        let scope = flywheel_test_sandbox::TestSandbox::new();'
