# Sourced by scripts/verify-mutations.sh.
# bd-ihiqo.5: ce-triage's crew comes from the workflow's `crew:` field.
# S1 drops the declaration, so nothing brings the crew up.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(purpose_named_workflows_are_crew_auto)

mutation "S1  ce-triage stops declaring its crew" \
    file=src/builtin_workflows.rs \
    expect=purpose_named_workflows_are_crew_auto \
    find='name: ce-triage
description: "Triage inbound work: acknowledge, robot-triage the backlog, prioritize and assign (deep), or quick-triage and hand off. Replaces triage and triage-deep."
trigger:
  type: message_posted
  channel: "#support"
crew: auto' \
    replace='name: ce-triage
description: "Triage inbound work: acknowledge, robot-triage the backlog, prioritize and assign (deep), or quick-triage and hand off. Replaces triage and triage-deep."
trigger:
  type: message_posted
  channel: "#support"'
