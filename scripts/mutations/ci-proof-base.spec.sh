#!/usr/bin/env bash
# bd-n6kyt/bd-b1j9d: the CI mutation base must come from the newest green
# Proof receipt, discovered through gh, not from the fallback pin.
set -uo pipefail

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
  ci_proof_base_prefers_the_newest_green_receipt
  ci_proof_base_discovers_a_green_receipt_through_gh
)

mutation "B1  CI always uses the pinned fallback base" \
  file=scripts/ci-proof-base.py \
  expect=ci_proof_base_prefers_the_newest_green_receipt \
  find='    eligible = [' \
  replace='    eligible = []  #'

mutation "B2  discovery ignores the green receipt and prints the fallback" \
  file=scripts/ci-proof-base.py \
  expect=ci_proof_base_discovers_a_green_receipt_through_gh \
  find='    print(pick_base(prepared, fallback))' \
  replace='    print(fallback)'
