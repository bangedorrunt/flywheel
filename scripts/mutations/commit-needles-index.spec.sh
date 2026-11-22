#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-npaqu: reading the working tree instead of the commit tree must turn the
# index-tree needle witness RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(commit_needles_read_the_index_tree)

mutation "N1  the fence lints the working tree again" \
    file=scripts/fence.sh \
    expect=commit_needles_read_the_index_tree \
    find='bash scripts/verify-needles.sh --repo "$DEST"' \
    replace='bash scripts/verify-needles.sh'
