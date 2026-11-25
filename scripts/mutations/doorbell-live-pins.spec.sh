# Sourced by scripts/verify-mutations.sh.
# bd-mjjgd: the doorbell sweep reads the rex roster only for waiting rings, at most every LIVE_PINS_EVERY_SECS.

CARGO_ARGS=(-p doorbell-rt)
TEST_FILTER=(
    sweep_reads_live_pins_only_for_waiting_rings_and_throttled
)

mutation "P1  live_pins read on every sweep even with no ring" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=sweep_reads_live_pins_only_for_waiting_rings_and_throttled \
    find='    has_rings && last_read' \
    replace='    true || last_read'

mutation "P1  live_pins read every sweep while a ring waits" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=sweep_reads_live_pins_only_for_waiting_rings_and_throttled \
    find='now.saturating_sub(last) >= LIVE_PINS_EVERY_SECS' \
    replace='now.saturating_sub(last) >= 0'
