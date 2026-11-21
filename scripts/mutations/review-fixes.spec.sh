# Sourced by scripts/verify-mutations.sh. ce-review follow-ups bd-vx9pv/bd-s3hzg.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_pointer_brief_has_no_literal_peer_token coordination_notice_key_is_project_scoped nested_steps_are_bound_and_waits_are_previewed)

mutation "R1  pointer brief leaks the literal peers token" \
    file=src/durable.rs \
    expect=a_pointer_brief_has_no_literal_peer_token \
    find=$'        .replace("{verb}", verb)\n        .replace("{id}", id)\n        .replace("{project}", project)\n        .replace("{peers}", &render_peer_contacts(board, pin))' \
    replace='.replace("{peers}", "{peers}")'
