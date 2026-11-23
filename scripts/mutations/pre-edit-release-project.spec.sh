#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-0uxg6: release pre-edit leases under the project the commit ran in.

CARGO_ARGS=(-p flywheel --test pre_edit_release)
TEST_FILTER=(commit_releases_under_the_commit_project_not_the_seat_project)

mutation "P1  release falls back to the seat's session project" \
    file=src/cmd/commit.rs \
    expect=commit_releases_under_the_commit_project_not_the_seat_project \
    find='.then(|| ctx.project.clone())' \
    replace='.then(|| String::new())'
