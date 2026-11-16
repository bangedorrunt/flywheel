# Sourced by scripts/verify-mutations.sh.
# bd-6setu.3 (ADR-0022 D1): the desired seats are the FOLD of the plan record's
# base and its journal events. Fold an empty event list and the replay silently
# answers with the base plan - a plan nobody changed.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(the_fold_rebuilds_the_desired_seats_from_the_journal)

mutation "F1  the fold ignores the journal's events" \
    file=src/cmd/swarm.rs \
    expect=the_fold_rebuilds_the_desired_seats_from_the_journal \
    find='    for event in &record.patches {' \
    replace='    for event in &record.patches[..0] {'
