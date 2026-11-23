#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-lk8s2: both release sites are proven by behaviour tests with a fake
# toron recording its argv, never by grepping the source text.

CARGO_ARGS=(-p flywheel --test pre_edit_release)
TEST_FILTER=(
    commit_releases_only_the_committed_paths
    crew_done_releases_the_brief_pins_fallback_leases
)

mutation "R1  commit skips the pre-edit release call but still compiles" \
    file=src/cmd/commit.rs \
    expect=commit_releases_only_the_committed_paths \
    find='        && !pin.is_empty()' \
    replace='        && pin.is_empty()'

mutation "R2  crew done releases the fallback leases under the wrong pin" \
    file=src/cmd/crew.rs \
    expect=crew_done_releases_the_brief_pins_fallback_leases \
    find='crate::pre_edit::release_all_fallback(project, &pin)' \
    replace='crate::pre_edit::release_all_fallback(project, &String::new())'
