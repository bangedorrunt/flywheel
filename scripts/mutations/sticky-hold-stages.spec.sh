# Sourced by scripts/verify-mutations.sh.
# bd-mww6u: a released hold must carry its stage chain.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(sticky_hold_yields_to_a_live_fleet sticky_hold_yields_to_cross_lane_contention)

mutation "S1  the released hold drops its stages" \
    file=src/briefing.rs \
    expect=sticky_hold_yields_to_a_live_fleet \
    find='    b.proposal.stages = JOIN_STAGES.iter().map(|s| (*s).to_string()).collect();' \
    replace='    b.proposal.stages.clear();'
