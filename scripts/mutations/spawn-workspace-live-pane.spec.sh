# Sourced by scripts/verify-mutations.sh.
# bd-89q4r: a terminal label (crew OR the multiplexer's own shell) outlives its
# pane. Only a live CREW pane makes a workspace a fleet to join; a stale label
# or a shell-only workspace must let the next spawn proceed.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(labelled_agent_without_a_live_pane_does_not_join shell_only_workspace_classifies_as_add_with_a_live_pane)

mutation "W1  a stale agent label counts as a live seat" \
    file=src/main.rs \
    expect=labelled_agent_without_a_live_pane_does_not_join \
    find='a.workspace_id == ws.workspace_id && is_crew(a) && live_panes.contains(&a.pane_id)' \
    replace='a.workspace_id == ws.workspace_id && is_crew(a)'

mutation "W2  the multiplexer shell counts as a crew seat" \
    file=src/main.rs \
    expect=shell_only_workspace_classifies_as_add_with_a_live_pane \
    find='.is_some_and(|n| !n.is_empty() && n != "shell")' \
    replace='.is_some_and(|n| !n.is_empty())'
