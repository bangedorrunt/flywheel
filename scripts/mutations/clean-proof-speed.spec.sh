#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-p7oim: each proof step carries its own wall time in the receipt.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_receipt_records_each_step_seconds proof_sibling_suite_caches_by_rev)

mutation "T1  the receipt drops per-step seconds" \
    file=scripts/proof-lib.sh \
    expect=proof_receipt_records_each_step_seconds \
    find='        step["secs"] = int(secs[i])' \
    replace='        pass  # bd-p7oim mutation: no timings'

mutation "C1  a sibling suite cache never hits" \
    file=scripts/clean-proof.sh \
    expect=proof_sibling_suite_caches_by_rev \
    find='if [ -n "$sib_rev" ] && [ -f "$cache_file" ]; then' \
    replace='if false; then # bd-p7oim mutation: re-run every sibling suite'
