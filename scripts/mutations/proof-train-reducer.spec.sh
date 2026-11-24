#!/usr/bin/env bash
# bd-7qhz0: the proof train is a pure reducer and landing takes the newest
# green receipt covering a bead. These mutants restore the old behaviours.
set -uo pipefail

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
  newest_green_receipt_does_not_need_a_fresh_proof
  ci_receipt_marks_skipped_jobs_failed_when_build_fails
)

mutation "L1  landing takes the OLDEST green receipt" \
  file=src/cmd/ship.rs \
  expect=newest_green_receipt_does_not_need_a_fresh_proof \
  find='        .max_by(|a, b| a.2.cmp(b.2))' \
  replace='        .min_by(|a, b| a.2.cmp(b.2))'

mutation "R1  a skipped CI job reads as green" \
  file=scripts/merge-proof-receipt.py \
  expect=ci_receipt_marks_skipped_jobs_failed_when_build_fails \
  find='    return False' \
  replace='    return True'
