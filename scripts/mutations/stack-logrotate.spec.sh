#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-9zw9u: skipping the plist rewrite must turn the fixture RED.

CARGO_ARGS=(-p flywheel --test stack_logrotate)
TEST_FILTER=(stack_logrotate_rewrite_and_status)

mutation "L1  install skips the logrotate plist rewrite" \
    file=scripts/stack.sh \
    expect=stack_logrotate_rewrite_and_status \
    find='  rewrite_logrotate_plist' \
    replace='  : # mutation: no side plist rewrite'
