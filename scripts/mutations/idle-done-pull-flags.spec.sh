#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-idle-done-wake-not-delivered-gnjok: an immediate inject to an idle/done
# pane must name the exact read command, not a bare `toron next`.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(doorbell_wake_delivery)

mutation "P1  immediate inject keeps the bare toron next" \
    file=crates/doorbell-core/src/lib.rs \
    expect=doorbell_wake_delivery_idle_line_names_the_read_command \
    find='    let line = with_read_flags(line, &project, pin);' \
    replace='    let line = line;'
