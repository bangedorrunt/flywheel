# Sourced by scripts/verify-mutations.sh.
# bd-8uozb: a dead push must re-arm with backoff and drop to the 3 s poll.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    a_push_stream_that_ends_rearms_and_restores_the_fast_poll
    rearm_backoff_is_bounded
)

HOST=src/doorbell_host.rs

mutation "R1  a dead push stream is never re-armed" \
    file=$HOST \
    expect=a_push_stream_that_ends_rearms_and_restores_the_fast_poll \
    find='        sleep(push_rearm_backoff(failures));
        failures = failures.saturating_add(1);' \
    replace='        return;'

mutation "R2  the slow poll persists after push dies" \
    file=$HOST \
    expect=a_push_stream_that_ends_rearms_and_restores_the_fast_poll \
    find='    if push_enabled && push_alive { 30 } else { 3 }' \
    replace='    if push_enabled { 30 } else { 3 }'
