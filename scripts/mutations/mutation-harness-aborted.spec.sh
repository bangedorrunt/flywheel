# Sourced by scripts/verify-mutations.sh.
# An aborted run is its own verdict (bd-0uzrq.24 / proof33). Dropping the branch
# makes a live breach read as NO-RESULTS again, which sends the reader hunting a
# CARGO_ARGS bug that does not exist.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_mutation_harness)

mutation "A1  an aborted run reads as a package mistake" \
    file=scripts/verify-mutations.sh \
    expect=proof_mutation_harness_classifies_an_abort \
    find='    if is_aborted "$out"; then' \
    replace='    if false; then'
