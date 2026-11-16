# Sourced by scripts/verify-mutations.sh.
# ADR-0021 D6 (bd-jhz8x.4): an unused stage fails verify, and a declaration
# (`reached_by = compose|manual`) is what keeps a manual/compose-only stage
# alive. U1 drops the declarations, so the five legitimate ones read as dead and
# the shipped tiers fail.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(statechart_verify_passes_the_shipped_charts)

mutation "U1  a declared reach is not counted as a reference" \
    file=src/cmd/workflow.rs \
    expect=statechart_verify_passes_the_shipped_charts \
    find='        referenced.extend(lib.declared_reach());' \
    replace='        let _ = lib.declared_reach();'
