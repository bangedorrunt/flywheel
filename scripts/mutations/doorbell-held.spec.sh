# Sourced by scripts/verify-mutations.sh.
#
# bd-0uzrq.8, the second half: a prompt the WORKSPACE cannot type is held in the
# doorbell queue, not turned into an audit row. The live stall (01:12 -> 01:14:41)
# was a held wake that had nowhere to wait, so it rang only if the doorbell's own
# status map later transitioned — minutes later, or never.
#
#   scripts/verify-mutations.sh scripts/mutations/doorbell-held.spec.sh --cargo mbx
#
# What this does not prove: the ring composition itself (doorbell-merge.spec.sh).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(a_held_prompt_is_queued_not_dropped)

WORKSPACE=src/workspace.rs

mutation "H1  a held prompt is audited away instead of queued" \
    file=$WORKSPACE \
    expect=a_held_prompt_is_queued_not_dropped \
    find='    for (pin, line) in held {' \
    replace='    for (pin, line) in Vec::<(String, String)>::new() {'

mutation "H2  a held prompt is promoted to a busy pane anyway" \
    file=$WORKSPACE \
    expect=a_held_prompt_is_queued_not_dropped \
    find='                } else {
                    held.push((pin, line));
                }' \
    replace='                } else {
                    keep.push(Cmd::Prompt { pin, line });
                }'
