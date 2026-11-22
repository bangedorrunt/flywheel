#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-lk0kt: dropping the queue emission must turn the statechart witness RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(pre_edit_message_log_replays_to_one_lease_and_one_write)

mutation "E1  a blocked pre-edit does not queue its waiter" \
    file=src/elm/pre_edit_lease_statechart.rs \
    expect=pre_edit_message_log_replays_to_one_lease_and_one_write \
    find='                        q.exec(PreEditCmd::Queue {' \
    replace='                        q.exec(PreEditCmd::Acquire {'
