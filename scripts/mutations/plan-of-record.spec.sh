# Sourced by scripts/verify-mutations.sh.
# bd-6setu.3 (ADR-0022 D1): the plan of record counts what the JOURNAL plans.
# Report the fleet's count instead and a seat the journal still plans vanishes
# from the view - the exact divergence the record exists to show.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(the_plan_of_record_counts_the_journal_not_the_fleet)

mutation "R1  the plan of record reports the fleet's seat count" \
    file=src/cmd/swarm.rs \
    expect=the_plan_of_record_counts_the_journal_not_the_fleet \
    find='        planned: fold_plan(record)?.len(),' \
    replace='        planned: live.len(),'
