# Sourced by scripts/verify-mutations.sh.
# bd-0kxzw: the live guard must prove the capture and send block ids, so a
# misdelivery or a stale pane label fails loudly instead of flaking.

CARGO_ARGS=(-p flywheel --test live_doorbell_operator_guard)
TEST_FILTER=(
    the_capture_block_must_resolve_to_one_live_block
    a_send_to_the_wrong_pane_fails_loudly
)

mutation "B1  the capture block resolves without a uniqueness check" \
    file=tests/live_doorbell_operator_guard.rs \
    expect=the_capture_block_must_resolve_to_one_live_block \
    find='    match hits.as_slice() {
        [only] => Ok((*only).to_string()),' \
    replace='    match hits.as_slice() {
        [first, ..] => Ok((*first).to_string()),'

mutation "B2  the send target is never checked" \
    file=tests/live_doorbell_operator_guard.rs \
    expect=a_send_to_the_wrong_pane_fails_loudly \
    find='    if pane != expected_block_id {' \
    replace='    if false && pane != expected_block_id {'
