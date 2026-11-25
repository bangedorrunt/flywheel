# Sourced by scripts/verify-mutations.sh.
# bd-k7b7h: pure diagnose_fleet rules in src/snapshot.rs catch all 4 v1 fleet incidents.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    diagnose_fleet_rex_version_skew_is_p0
    diagnose_fleet_stale_doorbell_generation_is_p1_and_equal_mtime_is_ok
    diagnose_fleet_host_log_over_cap_is_p2_and_at_cap_is_ok
    diagnose_fleet_chie_slow_and_unreachable_are_p1_and_250ms_is_ok
)

mutation "P0  rex skew rule never fires" \
    file=src/snapshot.rs \
    expect=diagnose_fleet_rex_version_skew_is_p0 \
    find='    if let Some(method) = &signals.rex_missing_method {' \
    replace='    if let Some(method) = Option::<&String>::None {'

mutation "P1  stale doorbell generation comparison inverted" \
    file=src/snapshot.rs \
    expect=diagnose_fleet_stale_doorbell_generation_is_p1_and_equal_mtime_is_ok \
    find='    ) && disk > running' \
    replace='    ) && disk < running'

mutation "P2  host log cap comparison treats at-cap as over-cap" \
    file=src/snapshot.rs \
    expect=diagnose_fleet_host_log_over_cap_is_p2_and_at_cap_is_ok \
    find='    if signals.host_log_cap_bytes > 0 && signals.host_log_max_bytes > signals.host_log_cap_bytes {' \
    replace='    if signals.host_log_cap_bytes > 0 && signals.host_log_max_bytes >= signals.host_log_cap_bytes {'

mutation "P1  chie slow threshold treats 250ms as slow" \
    file=src/snapshot.rs \
    expect=diagnose_fleet_chie_slow_and_unreachable_are_p1_and_250ms_is_ok \
    find='            .is_some_and(|ms| ms > CHIE_SLOW_THRESHOLD_MS)' \
    replace='            .is_some_and(|ms| ms >= CHIE_SLOW_THRESHOLD_MS)'
