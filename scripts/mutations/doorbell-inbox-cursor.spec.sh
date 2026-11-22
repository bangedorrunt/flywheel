#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-ba4nl: an inbox row below the pull cursor must queue nothing.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(an_inbox_row_below_the_cursor_queues_nothing)

mutation "C1  pulled inbox mail still rings" \
    file=crates/doorbell-core/src/lib.rs \
    expect=an_inbox_row_below_the_cursor_queues_nothing \
    find='    created_us.is_some_and(|us| already_pulled(&model.pulled_us, pin, us))' \
    replace='    false'
