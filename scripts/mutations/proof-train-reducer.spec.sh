#!/usr/bin/env bash
# bd-7qhz0: the proof train is a pure reducer and landing takes the newest
# green receipt covering a bead. These mutants restore the old behaviours.
set -uo pipefail

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
  a_new_head_during_a_run_starts_after_it_ends
  newest_green_receipt_does_not_need_a_fresh_proof
)

mutation "T1  a newer HEAD during a run is forgotten" \
  file=src/proof_train.rs \
  expect=a_new_head_during_a_run_starts_after_it_ends \
  find='            if let Some(head) = m.seen_head.clone()' \
  replace='            if let Some(head) = Option::<String>::None'

mutation "L1  landing takes the OLDEST green receipt" \
  file=src/cmd/ship.rs \
  expect=newest_green_receipt_does_not_need_a_fresh_proof \
  find='        .max_by(|a, b| a.2.cmp(b.2))' \
  replace='        .min_by(|a, b| a.2.cmp(b.2))'
