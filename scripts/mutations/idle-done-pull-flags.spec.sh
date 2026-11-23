#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# The inbox arm must render the wake with its reader, not a generic line.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(doorbell_wake_delivery)

mutation "P1  inbox wakes lose their reader context" \
    file=crates/doorbell-core/src/lib.rs \
    expect=doorbell_wake_delivery_idle_line_names_the_read_command \
    find='        DoorbellMsg::InboxRows { rows, pin } => {
            for item in rows {
                let Some((class, hint)) = parse_inbox_item(item) else {
                    continue;
                };
                let line_out = doorbell_line_for(class, &hint, &next.project, pin);' \
    replace='        DoorbellMsg::InboxRows { rows, pin } => {
            for item in rows {
                let Some((class, hint)) = parse_inbox_item(item) else {
                    continue;
                };
                let line_out = doorbell_line(class, &hint);'
