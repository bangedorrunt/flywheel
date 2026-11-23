#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-p7oim: each proof step carries its own wall time in the receipt.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_receipt_records_each_step_seconds proof_sibling_suite_caches_by_rev proof_suites_run_concurrently proof_changed_file_tests_run_first)

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

mutation "D1  the suites run one after the other again" \
    file=scripts/clean-proof.sh \
    expect=proof_suites_run_concurrently \
    find='( "$@" >"$TMP/$name.log" 2>&1; echo $? >"$TMP/$name.status" ) &' \
    replace='( "$@" >"$TMP/$name.log" 2>&1; echo $? >"$TMP/$name.status" )'

mutation "E1  changed-file tests are skipped" \
    file=scripts/clean-proof.sh \
    expect=proof_changed_file_tests_run_first \
    find='run_changed_file_tests() {
  local file name' \
    replace='run_changed_file_tests() {
  return 0
  local file name'
