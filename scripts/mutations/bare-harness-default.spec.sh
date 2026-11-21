# Sourced by scripts/verify-mutations.sh.
# bd-iu00a: a bare open harness means harness-default, never a slot.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    bare_open_harness_plans_harness_default
    objective_model_intent_maps_bare_and_explicit
    slot_alias_requires_at_and_maps_omp_names
)

mutation "U1  bare fallback restores the implied slot" \
    file=src/cmd/spawn.rs \
    expect=bare_open_harness_plans_harness_default \
    find='    if crate::harness_registry::tier_of(harness).is_some_and(|t| t.may_be_asked_for_a_model()) {
        return Some((
            ModelChoice::HarnessDefault,
            crate::swarm_plan::Provenance::HarnessDefault,
        ));
    }' \
    replace='    if false {
        return None;
    }'

mutation "U2  the objective parser ignores bare/explicit model intent" \
    file=src/briefing.rs \
    expect=objective_model_intent_maps_bare_and_explicit \
    find='pub fn model_intent_from_objective(objective: &str) -> Option<String> {
    let lower = objective.to_ascii_lowercase();' \
    replace='pub fn model_intent_from_objective(objective: &str) -> Option<String> {
    if true { return None; }
    let lower = objective.to_ascii_lowercase();'

mutation "U3  @default maps back to the standard slot" \
    file=src/models.rs \
    expect=slot_alias_requires_at_and_maps_omp_names \
    find='        // bd-iu00a: "default" is NOT a slot alias. A bare harness means its
        // own model (HARNESS_DEFAULT); a slot must be named explicitly.
        _ => None,' \
    replace='        "default" => Some(SLOT_STANDARD),
        _ => None,'
