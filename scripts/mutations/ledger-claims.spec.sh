#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-sy7vn: a claim to close is a `Closes:`/`Closed:` trailer, never a bead id
# in prose. Both directions are pinned by the guard's own self-test.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(clean_proof)

GUARD=scripts/check-beads-ledger.sh

mutation "L1  prose counts as a claim to close again" \
    file=$GUARD \
    expect=the_beads_ledger_guard_reads_trailers_not_prose \
    find="  grep -oE '^(Closes|Closed):[[:space:]].*\$'" \
    replace="  grep -oE 'close[[:space:]].*'"

mutation "L2  the trailer is not read at all" \
    file=$GUARD \
    expect=the_beads_ledger_guard_reads_trailers_not_prose \
    find="  grep -oE '^(Closes|Closed):[[:space:]].*\$'" \
    replace="  grep -oE '^$'"
