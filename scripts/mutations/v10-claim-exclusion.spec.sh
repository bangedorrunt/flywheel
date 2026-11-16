# Sourced by scripts/verify-mutations.sh.
# bd-6setu.6: only a review step's `steps.*` reads become claim exclusions.
# F1 lets `inputs.*` through, which would exclude a node that does not exist.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_review_steps_reads_ride_its_job_spec)

mutation "F1  an inputs.* ref becomes a claim exclusion" \
    file=src/workflow_lowering.rs \
    expect=a_review_steps_reads_ride_its_job_spec \
    find='                .filter_map(|r| r.strip_prefix("steps.").map(str::to_string))' \
    replace='                .map(|r| r.strip_prefix("steps.").unwrap_or(r).to_string())'
