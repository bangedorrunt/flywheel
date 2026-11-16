# Sourced by scripts/verify-mutations.sh.
# A seat whose pane the plane no longer lists is already closed; closing it
# again errors and strands the pin before the lease release (bd-0uzrq.21).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(cmd::swarm::tests::retire_gone_seat_counts_as_closed_and_retire_completes)

mutation "G1  an absent pane is closed again" \
    file=src/supervisor_host.rs \
    expect=cmd::swarm::tests::retire_gone_seat_counts_as_closed_and_retire_completes \
    find='        if live.as_ref().is_none_or(|set| set.contains(&pane)) {' \
    replace='        if true {'
