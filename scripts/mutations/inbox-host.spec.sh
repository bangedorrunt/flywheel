# Sourced by scripts/verify-mutations.sh.
# A restart must not refresh the ring budget (bd-m7gk follow-up b).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(inbox_host::tests::a_restart_does_not_refresh_the_ring_budget)

mutation "I1  restore refreshes rings_since_advance" \
    file=src/inbox_host.rs \
    expect=a_restart_does_not_refresh_the_ring_budget \
    find='            r.rings_since_advance = b.rings_since_advance;' \
    replace='            r.rings_since_advance = 0;'
