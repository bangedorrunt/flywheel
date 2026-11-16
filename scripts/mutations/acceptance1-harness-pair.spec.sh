# Sourced by scripts/verify-mutations.sh.
# bd-6setu.10 (ADR-0022 acceptance 1): fx has no model flag, so an explicit model
# ask on it is BLOCKED. Give fx the flag and the plan silently claims delivery -
# the false green acceptance 1 exists to catch.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(acceptance1_gemini_honored_fx_blocked)

mutation "A1  fx grows a model flag it does not have" \
    file=src/headless.rs \
    expect=acceptance1_gemini_honored_fx_blocked \
    find='    let Some(flag) = row.model_flag else {
        return vec![];
    };' \
    replace='    let Some(flag) = Some("-m".to_string()) else {
        return vec![];
    };'

