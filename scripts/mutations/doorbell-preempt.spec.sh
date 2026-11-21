# Sourced by scripts/verify-mutations.sh.
#
# bd-0uzrq.13: a peer request must never be typed into a WORKING pane, and a
# wake that does preempt must ring exactly once. The live signature: peer
# Prompts at 05:29:41, 09:24:43 and 10:58:15Z, each emitted while the pin was
# working, each swallowed by the harness, each re-rung from the CLI's own
# deferred copy at the drain.
#
#   scripts/verify-mutations.sh scripts/mutations/doorbell-preempt.spec.sh --cargo mbx
#
# What this does not prove: the CLI's PromptRefused producer (it is exercised by
# doorbell-merge.spec.sh's two-producer arm) and the live pane behaviour — the
# captain runs that leg.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(tests::)

CORE=crates/doorbell-core/src/lib.rs

mutation "W13  a peer request counts as act-now and preempts the working pane" \
    file=$CORE \
    expect=a_peer_request_waits_for_the_prompt_instead_of_preempting_it \
    find='    line.contains("Urgent mail") || line.contains("URGENT steer")
}' \
    replace='    is_urgent_line(line)
}'

mutation "W14  the preempt does not record its key, so the re-read rings again" \
    file=$CORE \
    expect=an_act_now_preempt_records_its_key_so_the_reread_is_silent \
    find='            if !key.is_empty() {
                next.rung.insert(key);
            }
            actions.push(DoorbellAction::Prompt {' \
    replace='            actions.push(DoorbellAction::Prompt {'

mutation "W15  the already-rung guard is consulted only for injectable panes" \
    file=$CORE \
    expect=an_act_now_preempt_records_its_key_so_the_reread_is_silent \
    find='    if !key.is_empty() && next.rung.contains(&key) {' \
    replace='    if should_inject(status) && !key.is_empty() && next.rung.contains(&key) {'
