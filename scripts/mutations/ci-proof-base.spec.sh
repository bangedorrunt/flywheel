#!/usr/bin/env bash
# bd-n6kyt: the CI mutation base must come from the newest green receipt.
set -uo pipefail

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(ci_proof_base_prefers_the_newest_green_receipt)

mutation "B1  CI always uses the pinned fallback base" \
  file=scripts/ci-proof-base.py \
  expect=ci_proof_base_prefers_the_newest_green_receipt \
  find='    eligible = [' \
  replace='    eligible = []  #'
