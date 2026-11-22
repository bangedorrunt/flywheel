#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-jp9z1.3: the cheapest check that can fail runs first. A rotted needle must
# fail in under a second, a clippy error before the suite, and a spec edited in
# this landing before the ones that have not moved.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(clean_proof)

PROOF=scripts/clean-proof.sh
NEEDLES=scripts/verify-needles.sh

mutation "N1  the needle lint is dropped from the proof" \
    file=$PROOF \
    expect=clean_proof_runs_needles_then_build_then_clippy_then_suite \
    find='run_step verify-needles bash scripts/verify-needles.sh || true' \
    replace='true'

mutation "N2  clippy is dropped from before the suite" \
    file=$PROOF \
    expect=clean_proof_runs_needles_then_build_then_clippy_then_suite \
    find='[ -n "$FAILED_STEP" ] || run_step verify-clippy bash scripts/verify-clippy.sh || true' \
    replace='true  # mutation: clippy dropped from before the suite'

mutation "N3  the lint accepts a needle that matches nothing" \
    file=$NEEDLES \
    expect=the_needle_lint_fails_and_names_spec_and_id_when_a_needle_is_gone \
    find='    if occurrences != 1:' \
    replace='    if occurrences > 1:'

mutation "N4  the lint accepts a needle that occurs twice" \
    file=$NEEDLES \
    expect=the_needle_lint_fails_when_a_needle_occurs_twice \
    find='    if occurrences != 1:' \
    replace='    if occurrences > 2:'

mutation "N5  the lint stops checking that the target exists" \
    file=$NEEDLES \
    expect=the_needle_lint_reports_a_missing_target_file \
    find='    if not target.is_file():' \
    replace='    if False:'

mutation "N6  mutation specs run oldest change first" \
    file=$PROOF \
    expect=mutation_specs_run_newest_change_first \
    find='-k1,1nr -k2,2' \
    replace='-k1,1n -k2,2'

mutation "N7  the mutation loop ignores the recency order" \
    file=$PROOF \
    expect=mutation_specs_run_newest_change_first \
    find='done < <(ordered_specs)' \
    replace='done < <(printf "%s\n" scripts/mutations/*.spec.sh)'

mutation "N8  the proof runs the working tree's driver again" \
    file=$PROOF \
    expect=a_sha_proof_runs_the_revisions_own_driver \
    find='  if [ -n "$DRIVER_ARG" ]; then' \
    replace='  if false; then'
