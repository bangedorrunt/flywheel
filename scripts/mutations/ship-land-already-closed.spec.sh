# Sourced by scripts/verify-mutations.sh.
# bd-j7hod: a replay at the same sha must report already landed, not fail at
# the gate. Removing the short-circuit turns the replay witness RED.

CARGO_ARGS=(-p flywheel --test ship_land)
TEST_FILTER=(land_on_an_already_closed_bead_at_the_same_sha_is_already_landed)

mutation "J1  no already-landed short-circuit" \
    file=src/cmd/ship.rs \
    expect=land_on_an_already_closed_bead_at_the_same_sha_is_already_landed \
    find='if bead_closed(&dir, bead)? && bead_rows_committed(&dir, bead)? {' \
    replace='if false {'
