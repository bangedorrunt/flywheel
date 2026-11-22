#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-5b8ak: a deferred row without the verb field must turn the unit test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(a_deferred_request_release_carries_its_verb)

mutation "V1  deferred rows drop the structured verb" \
    file=src/doorbell.rs \
    expect=a_deferred_request_release_carries_its_verb \
    find='"verb": verb })' \
    replace='"wording": line })'
