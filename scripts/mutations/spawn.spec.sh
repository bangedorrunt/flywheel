# Sourced by scripts/verify-mutations.sh.
#
# Every dispatcher start site keys by the live session id, never the swarm label
# (bd-bj9xk).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(dispatcher_workspace_keys_by_the_session_id)

SPAWN=src/cmd/spawn.rs

mutation "S1  the dispatcher keys by the swarm label" \
    file=$SPAWN \
    expect=dispatcher_workspace_keys_by_the_session_id \
    find=$'    if ws.workspace_id.is_empty() {\n        label\n    } else {\n        ws.workspace_id.as_str()\n    }' \
    replace='    label'
