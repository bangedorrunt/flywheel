# Sourced by scripts/verify-mutations.sh.
#
# bd-0uzrq.17: the embedded dispatcher published PlaneStatus for every covered
# pin on every poll, so a bounded brain channel carried a backlog of stale
# statuses. Live (14:49-14:52Z) an idle pane stayed `working` for minutes and
# the deferred wake never drained. Transitions only.
#
#   scripts/verify-mutations.sh scripts/mutations/doorbell-plane-source.spec.sh --cargo mbx
#
# What this does not prove: the plane read itself (`agent_list` -> the one
# `block_state` detector). The live leg drives that end: an idle crew holding
# one unread is rung within about 10s plus a tick.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(presence_producer_tests)

HOST=src/doorbell_host.rs

mutation "P1  every cycle republishes the same status (the live backlog)" \
    file=$HOST \
    expect=only_a_status_change_reaches_the_brain \
    find='                    Some(prev) if prev == status => false,' \
    replace='                    Some(_prev) if false => false,'

mutation "P2  an idle flip is swallowed, so the queue never drains" \
    file=$HOST \
    expect=only_a_status_change_reaches_the_brain \
    find='                    Some(prev) if prev == status => false,' \
    replace='                    Some(_prev) if status == "idle" => false,'
