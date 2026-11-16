# Sourced by scripts/verify-mutations.sh.
# The guard CORE: a crew pane that unset AGENT_NAME is not the operator, and
# the pane marker that tells them apart is stamped at spawn (bd-0uzrq.24).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(an_unpinned_crew_pane env_marks_the_pane_for_the_fleet_guard)

mutation "P1  an unpinned pane inherits the operator's permission" \
    file=src/authority.rs \
    expect=an_unpinned_crew_pane_may_not_mutate_the_fleet \
    find='        if !in_pane {' \
    replace='        if true {'

mutation "P2  spawned panes carry no marker" \
    file=src/hc.rs \
    expect=env_marks_the_pane_for_the_fleet_guard \
    find='    env.insert("FLYWHEEL_PANE".to_string(), "1".to_string());' \
    replace='    // mutation: the pane marker is dropped'
