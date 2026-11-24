#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-zcm9j: a fresh opencode seat needs more than two minutes for its first turn.

CARGO_ARGS=(-p flywheel --test live_doorbell_operator_guard)
TEST_FILTER=(setup_wait_tests)

mutation "W1  the setup wait is back to two minutes" \
    file=tests/live_doorbell_operator_guard.rs \
    expect=first_turn_past_the_old_bound_still_reaches_the_guard \
    find='const SETUP_MAX_POLLS: usize = 120;' \
    replace='const SETUP_MAX_POLLS: usize = 24;'
