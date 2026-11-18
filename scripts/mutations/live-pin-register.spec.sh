# Sourced by scripts/verify-mutations.sh.
# bd-oo7zi: the live wake drives the send.rs door with an explicit pane target.

CARGO_ARGS=(--test live_doorbell_operator_guard)
TEST_FILTER=(the_live_wake_uses_send_with_a_pane_id_target)

mutation "R1  the send door is not given a pane target" \
    file=tests/live_doorbell_operator_guard.rs \
    expect=the_live_wake_uses_send_with_a_pane_id_target \
    find='        .args(["send", project, text, "--to", pane])' \
    replace='        .args(["send", project, text])'
