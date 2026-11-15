# Sourced by scripts/verify-mutations.sh.
#
# bd-0uzrq.13, the daemon door: the file host's presence filter asks the SAME
# question as the reducer — may this line preempt an occupied pane? — so it must
# read the SAME predicate. Mutating this call site back to the urgency
# classifier types a peer request into a busy pane again.
#
#   scripts/verify-mutations.sh scripts/mutations/doorbell-preempt-host.spec.sh --cargo mbx

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(file_host::tests::)

HOST=crates/doorbell-rt/src/file_host.rs

mutation "W16  the daemon door reads urgency instead of act-now" \
    file=$HOST \
    expect=busy_presence_holds_a_peer_request_for_the_prompt \
    find='                        urgent: doorbell_core::is_act_now_line(&line),' \
    replace='                        urgent: doorbell_core::is_urgent_line(&line),'
