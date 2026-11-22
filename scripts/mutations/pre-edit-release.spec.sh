#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-lk0kt: crew done must free fallback pre-edit leases.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(releases)

mutation "R2  close leaves fallback leases held" \
    file=src/cmd/crew.rs \
    expect=crew_done_releases_remaining_fallback_leases \
    find='crate::pre_edit::release_all_fallback(project, &pin)' \
    replace='crate::pre_edit::release_all_fallback(project, &String::new())'
