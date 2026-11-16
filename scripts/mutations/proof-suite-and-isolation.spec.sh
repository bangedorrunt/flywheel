# Sourced by scripts/verify-mutations.sh.
# The proof harness's own two rules (bd-jp9z1.5): the suite step executes the
# tests/ targets, and the mutation harness never edits the caller's tree.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_)

mutation "S1  the suite step only runs --bin flywheel" \
    file=scripts/clean-proof.sh \
    expect=proof_suite_step_runs_the_tests_directory \
    find='run_step suite "${MBX[@]}" test -p flywheel --tests' \
    replace='run_step suite "${MBX[@]}" test -p flywheel --bin flywheel'

mutation "I1  the mutation harness edits the caller tree" \
    file=scripts/verify-mutations.sh \
    expect=proof_verify_mutations_never_edits_the_shared_tree \
    find='    isolate_if_shared "$@"' \
    replace='    : isolate_if_shared "$@"'
