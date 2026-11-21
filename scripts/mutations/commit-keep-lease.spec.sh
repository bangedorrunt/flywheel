# Sourced by scripts/verify-mutations.sh.
# bd-u1n91: --keep-lease keeps the committed paths' leases. Dropping the guard
# makes every commit release, which the keep-lease witness must catch.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(keep_lease_leaves_the_committed_paths_held)

mutation "K1  commit releases even with --keep-lease" \
    file=src/cmd/commit.rs \
    expect=keep_lease_leaves_the_committed_paths_held \
    find=$'                ReservationMode::Enforce {\n                    keep_lease: false,\n                    ..\n                }' \
    replace=$'                ReservationMode::Enforce {\n                    keep_lease: _,\n                    ..\n                }'
