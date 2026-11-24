#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-ihiqo.6: old state-log shapes must be skipped without a warning.

CARGO_ARGS=(-p flywheel --test state_log_malformed)
TEST_FILTER=(malformed_log_lines_do_not_warn_on_a_normal_command)

mutation "M1  the loader warns on old log shapes again" \
    file=src/state.rs \
    expect=malformed_log_lines_do_not_warn_on_a_normal_command \
    find='        if let Some(entry) = parse_log_line(&line) {
            out.push(entry);
        }' \
    replace='        if let Some(entry) = parse_log_line(&line) {
            out.push(entry);
        } else {
            eprintln!("flywheel: skipping malformed log line: {line}");
        }'
