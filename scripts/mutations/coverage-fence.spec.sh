# Sourced by scripts/verify-mutations.sh.
# bd-jhz8x.7 (ADR-0021 D7 rule 3 / acceptance 4): a polish round may not drop a
# requirement from coverage without a non-goals entry. C1 disables the check;
# W1 lets the loop converge without it, so a shrinking round still ends the
# loop as a success.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    the_coverage_fence_seeds_once_and_never_shrinks
    ce_lfg_polish_requires_coverage
)

mutation "C1  a dropped requirement is not a coverage failure" \
    file=src/plan_fence.rs \
    expect=the_coverage_fence_seeds_once_and_never_shrinks \
    find='    let mut missing: Vec<String> = requirements
        .iter()
        .filter(|r| !covered.contains(*r) && !non_goals.contains(*r))
        .cloned()
        .collect();' \
    replace='    let mut missing: Vec<String> = Vec::new();'

mutation "W1  ce-lfg polish loop converges without coverage" \
    file=src/builtin_workflows.rs \
    expect=ce_lfg_polish_requires_coverage \
    find='    until: steps_polish_ok == true && steps_coverage_ok == true' \
    replace='    until: steps_polish_ok == true'
