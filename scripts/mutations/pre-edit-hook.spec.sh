#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-lk0kt: each installability fence must fail loudly when removed.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(pre_edit)

mutation "E1  a blocked pre-edit does not queue its waiter" \
    file=src/elm/pre_edit_lease_statechart.rs \
    expect=pre_edit_message_log_replays_to_one_lease_and_one_write \
    find='                        q.exec(PreEditCmd::Queue {' \
    replace='                        q.exec(PreEditCmd::Acquire {'

mutation "T1  reads take a lease too" \
    file=src/main.rs \
    expect=the_hook_filters_write_tools_at_the_door \
    find='                    if !crate::pre_edit::is_write_tool(&tool) {' \
    replace='                    if false {'

mutation "B1  a conflict lets the edit proceed" \
    file=src/pre_edit.rs \
    expect=a_held_path_denies_the_edit \
    find='    if holder.is_empty() { 0 } else { 2 }' \
    replace='    if holder.is_empty() { 0 } else { 0 }'
