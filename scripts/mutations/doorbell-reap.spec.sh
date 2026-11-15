# Sourced by scripts/verify-mutations.sh.
# A reap runs only on a rex-primary plane list (bd-bj9xk hazard).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(hc_fallback_list_with_session_doors_reaps_nothing)

mutation "D3  the reap trusts any plane list" \
    file=src/cmd/doorbell.rs \
    expect=hc_fallback_list_with_session_doors_reaps_nothing \
    find='    if live.plane != LivePlane::Rex {' \
    replace='    if false {'
