#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-9idgs: skipping broken-cfg discovery must turn the fixture RED.

CARGO_ARGS=(-p flywheel --test verify_formal_discovery)
TEST_FILTER=(formal_discovery_fixture)

mutation "F1  broken cfgs are not discovered" \
    file=scripts/verify-formal.sh \
    expect=formal_discovery_fixture \
    find='for cfg in "$MODELS"/*.broken-*.cfg; do' \
    replace='for cfg in ; do'
