# Sourced by scripts/verify-mutations.sh.
# bd-gtw4i: no scheduled catalog workflow may ship a shape its durable runner
# refuses. The guard walks the catalog and asks the durable model; this mutation
# makes weekly-review's fan-out count unresolvable at open, which is exactly the
# class of defect the guard exists to catch.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(every_scheduled_catalog_workflow)

mutation "G1  a scheduled workflow no longer opens on the durable models" \
    file=src/builtin_workflows.rs \
    expect=every_scheduled_catalog_workflow_opens_on_the_durable_models \
    find='    over: "3"
    each:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@scout"' \
    replace='    over: "not-a-count"
    each:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@scout"'
