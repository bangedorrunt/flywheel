# Sourced by scripts/verify-mutations.sh.
# bd-6setu.10 (ADR-0022 acceptance 1): fx has no model flag, so an explicit model
# ask on it is BLOCKED. Give fx the flag and the plan silently claims delivery -
# the false green acceptance 1 exists to catch.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(acceptance1_gemini_honored_fx_blocked)

mutation "A1  a gemini pane drops its interactive model argv" \
    file=src/headless.rs \
    expect=acceptance1_gemini_honored_fx_blocked \
    find='    InteractiveModelPlan::Argv(model_args(harness, model, effort))' \
    replace='    InteractiveModelPlan::HarnessDefault'

