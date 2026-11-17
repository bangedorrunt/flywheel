# Sourced by scripts/verify-mutations.sh.
# bd-8wdgn: a wake line that is already queued is a redundant RING, not a failed
# send - the mail is stored and the prompt must succeed with queued:true. Drop
# the phrase from the deferrable set and a scripted send exits non-zero.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(an_already_queued_wake_line_succeeds_with_queued_true)

mutation "Q1  an already-queued wake line fails the prompt" \
    file=src/cmd/crew.rs \
    expect=an_already_queued_wake_line_succeeds_with_queued_true \
    find='    err.contains("a wake line is already queued")' \
    replace='    err.contains("a wake line is already queued (never)")'
