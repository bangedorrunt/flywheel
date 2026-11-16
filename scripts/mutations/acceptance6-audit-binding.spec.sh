# Sourced by scripts/verify-mutations.sh.
# bd-6setu.13 (ADR-0022 acceptance 6): the lifecycle audit is a fresh reviewer
# dispatch, never the planner that polished. Give the audit the planner profile
# and the author audits its own work, which is the failure the row names.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(acceptance6_lifecycle_audit_binds_a_non_author_and_another_family)

mutation "A6  the lifecycle audit is dispatched to the planner" \
    file=src/builtin_workflows.rs \
    expect=acceptance6_lifecycle_audit_binds_a_non_author_and_another_family \
    find='        profile: "@reviewer"
        stage: plan-polish' \
    replace='        profile: "@planner"
        stage: plan-polish'
