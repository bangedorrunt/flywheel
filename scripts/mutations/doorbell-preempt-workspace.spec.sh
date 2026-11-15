# Sourced by scripts/verify-mutations.sh.
#
# bd-0uzrq.13, the workspace door: `filter_prompts` holds a wake that cannot be
# typed on a busy pin, and it must ask the same question as the reducer and the
# daemon — act-now, not urgency. Mutating it back to the urgency classifier keeps
# a peer request and types it into a working pane.
#
#   scripts/verify-mutations.sh scripts/mutations/doorbell-preempt-workspace.spec.sh --cargo mbx

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(filter_prompts_)

WORKSPACE=src/workspace.rs

mutation "W17  the workspace gate reads urgency instead of act-now" \
    file=$WORKSPACE \
    expect=filter_prompts_busy_urgent_breaks_through_rest_audits \
    find='                    || (is_act_now_line(&line)' \
    replace='                    || (is_urgent_line(&line)'
