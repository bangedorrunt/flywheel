# Sourced by scripts/verify-mutations.sh.
# bd-briefing-model-wrong-vehicle-fv0vh: a model ask only belongs on spawn/add.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    briefing_model_intent_never_splices_into_a_non_spawn_vehicle
    objective_model_intent_splices_as_an_explicit_user_ask
)

mutation "M1  model intent splices into every vehicle" \
    file=src/briefing.rs \
    expect=briefing_model_intent_never_splices_into_a_non_spawn_vehicle \
    find='fn splice_model_intent(argv: &mut Vec<String>, objective: &str) {
    if !model_vehicle(argv) {
        return;
    }' \
    replace='fn splice_model_intent(argv: &mut Vec<String>, objective: &str) {
    if false && !model_vehicle(argv) {
        return;
    }'
