# Sourced by scripts/verify-mutations.sh.
# bd-ihiqo.3: the work unit closes only on a green verify, and an answered
# collab ask never counts as an open wait. Dropping either fence must turn
# the matching test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(one_message_log_per_close_route_plus_handoff an_unverified_close_does_not_fire dead_waiters_and_answered_threads_stay_silent)

mutation "W1  verify never marks green" \
    file=src/elm/work_unit_statechart.rs \
    expect=one_message_log_per_close_route_plus_handoff \
    find='                        ctx.verified = true;' \
    replace='                        ctx.verified = false;'

mutation "W2  answered threads stay open" \
    file=src/cmd/deadlock.rs \
    expect=dead_waiters_and_answered_threads_stay_silent \
    find='    let answered = rows.iter().any(|(id, from, ts)| {
        *from == askee && Some(*id) != head && ask_ts.is_none_or(|hts| *ts > hts)
    });' \
    replace='    let answered = false;'
