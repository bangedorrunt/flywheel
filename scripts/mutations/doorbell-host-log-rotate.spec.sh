# Sourced by scripts/verify-mutations.sh.
# bd-tcaj0: spawn moves a host log over HOST_LOG_CAP_BYTES to .1 and keeps one old copy.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    host_log_rotates_when_over_cap_and_keeps_one_old_copy
)

mutation "P2  log at the cap is moved aside" \
    file=src/doorbell.rs \
    expect=host_log_rotates_when_over_cap_and_keeps_one_old_copy \
    find='m.len() > cap_bytes' \
    replace='m.len() >= cap_bytes'

mutation "P2  over-size log is never moved" \
    file=src/doorbell.rs \
    expect=host_log_rotates_when_over_cap_and_keeps_one_old_copy \
    find='        let _ = std::fs::rename(log, old);' \
    replace='        let _ = old;'
