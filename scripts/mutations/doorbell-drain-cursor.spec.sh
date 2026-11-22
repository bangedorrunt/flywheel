#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-ba4nl: the idle drain must re-check the pull cursor before ringing.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(a_mail_pulled_after_queueing_is_dropped_at_drain)

mutation "D1  the idle drain rings already-pulled mail" \
    file=crates/doorbell-core/src/lib.rs \
    expect=a_mail_pulled_after_queueing_is_dropped_at_drain \
    find='    Some(read.cursor_us.is_some_and(|cursor| created_us <= cursor))' \
    replace='    Some(false)'
