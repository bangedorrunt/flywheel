#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-6xu6r: one alert per break, and the error snippet must reach the owner.
# Both are behaviour assertions against a fake toron's argv log.

CARGO_ARGS=(--test blocked_by)
TEST_FILTER=(
    a_build_break_mails_its_holder_once
    an_error_in_the_callers_own_file_mails_no_one
)

mutation "B1  the dedupe journal check is skipped" \
    file=src/cmd/blocked_by.rs \
    expect=a_build_break_mails_its_holder_once \
    find='        if journal.contains(&key) {' \
    replace='        if false {'

mutation "B2  the alert body drops the compiler snippet" \
    file=src/cmd/build_break_statechart.rs \
    expect=a_build_break_mails_its_holder_once \
    find='                body: format!("build break in {file}\n{snippet}"),' \
    replace='                body: format!("build break in {file}"),'
