# Sourced by scripts/verify-mutations.sh. bd-ahkw1: commit refuses a live
# foreign lease, clears dead ones and releases its own on success.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_live_reservation_refuses_the_commit a_dead_lease_is_cleared_and_a_success_releases_own)

mutation "R1  commit ignores a live foreign lease" \
    file=src/cmd/commit.rs \
    expect=a_live_reservation_refuses_the_commit \
    find='    guard_commit_reservations(paths)?;' \
    replace='    let _ = paths;'

mutation "R2  a successful commit keeps its own leases" \
    file=src/cmd/commit.rs \
    expect=a_dead_lease_is_cleared_and_a_success_releases_own \
    find='            release_own_reservations(paths);' \
    replace='            let _ = paths;'
