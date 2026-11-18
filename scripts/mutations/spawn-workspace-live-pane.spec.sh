# Sourced by scripts/verify-mutations.sh.
# bd-89q4r: a terminal label outlives its pane. Counting an agent as live
# without checking the plane's pane_list makes a retired/crashed seat look like
# a fleet, so the next spawn refuses without --retire-orphans.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(labelled_agent_without_a_live_pane_does_not_join)

mutation "W1  a stale agent label counts as a live seat" \
    file=src/main.rs \
    expect=labelled_agent_without_a_live_pane_does_not_join \
    find='a.workspace_id == ws.workspace_id && live_panes.contains(&a.pane_id)' \
    replace='a.workspace_id == ws.workspace_id'
