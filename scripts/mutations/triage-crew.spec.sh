# Sourced by scripts/verify-mutations.sh.
# bd-jhz8x.2 (ADR-0021 D4.5): triage-deep's crew comes from the workflow's
# `crew:` field, which the pane door spawns, not from a seat that shells out to
# `flywheel spawn`. S1 drops the declaration, so nothing brings the crew up and
# the retired spawn-crew seat would be the only thing left doing it.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(triage_deep_crew_comes_from_the_workflow_not_a_seat)

mutation "S1  triage-deep stops declaring its crew" \
    file=src/builtin_workflows.rs \
    expect=triage_deep_crew_comes_from_the_workflow_not_a_seat \
    find='name: triage-deep
description: "Deep backlog triage — bv robot-triage, prioritize + assign; the crew comes from this workflow'"'"'s `crew:` field (bd-jhz8x.2 retired the spawn-crew seat). NOTE: /triage -> triage-deep is intentional (triage = auto #support trigger; triage-deep = interactive)."
trigger:
  type: webhook
crew: auto' \
    replace='name: triage-deep
description: "Deep backlog triage — bv robot-triage, prioritize + assign; the crew comes from this workflow'"'"'s `crew:` field (bd-jhz8x.2 retired the spawn-crew seat). NOTE: /triage -> triage-deep is intentional (triage = auto #support trigger; triage-deep = interactive)."
trigger:
  type: webhook'
