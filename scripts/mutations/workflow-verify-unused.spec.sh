# Sourced by scripts/verify-mutations.sh.
# bd-jhz8x.2: an unreferenced stage is REPORTED, never fatal — the catalog is
# also the manual dispatch surface, and failing on it made `workflow verify`
# read the operator's run log. U1 makes it fatal again.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(statechart_verify_passes_the_shipped_charts)

mutation "U1  an unreferenced stage fails workflow verify again" \
    file=src/cmd/workflow.rs \
    expect=statechart_verify_passes_the_shipped_charts \
    find='    let ok = reports.iter().all(|r| r.ok);' \
    replace='    let ok = reports.iter().all(|r| r.ok) && unused.is_empty();'
