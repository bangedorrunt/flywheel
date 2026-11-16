# Sourced by scripts/verify-mutations.sh.
# bd-gtw4i: every published workflow must open on the durable models unless it
# is legacy by design (branch/loop graph nodes). This mutation makes a fan-out
# count unresolvable at open — the class that shipped three unopenable
# workflows (weekly-review, debate, interrogate).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(every_published_workflow every_scheduled_catalog_workflow)

mutation "G1  a published workflow no longer opens on the durable models" \
    file=src/builtin_workflows.rs \
    expect=every_published_workflow_is_durable_or_legacy_by_design \
    find='    over: "2"
    each: { action: dispatch_job, assignee: x, spec: { profile: "@reviewer", harness: "opencode", task: "Brief on the proposal from your adversarial angle: {{inputs.proposal}}" } }' \
    replace='    over: "not-a-count"
    each: { action: dispatch_job, assignee: x, spec: { profile: "@reviewer", harness: "opencode", task: "Brief on the proposal from your adversarial angle: {{inputs.proposal}}" } }'
