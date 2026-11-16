# Sourced by scripts/verify-mutations.sh.
# bd-6setu.3 (ADR-0022 D1): a retire is a plan event. Drop the writer and the
# fold keeps planning a seat the operator removed, so the next apply resurrects
# the pin.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(retire_journals_the_plan_event)

mutation "T1  retire dispatches its seat effect but writes no plan event" \
    file=src/cmd/swarm.rs \
    expect=retire_journals_the_plan_event \
    find='        crate::state::append_plan_event(project, &format!("retire:{pin}"))?;' \
    replace='        // mutation: the retire forgets its plan event'
