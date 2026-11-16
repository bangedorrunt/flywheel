# Sourced by scripts/verify-mutations.sh.
# bd-0uzrq.23: a cold start never replays the whole retention, and a slow brain
# parks the forwarder instead of shedding the line and condemning the consumer.

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(
    a_cold_start_cursor_is_a_bounded_lookback_not_origin
    a_replay_larger_than_the_channel_neither_drops_nor_ends_the_consumer
)

mutation "M1  cold start resumes from origin" \
    file=crates/doorbell-rt/src/wake_brain.rs \
    expect=a_cold_start_cursor_is_a_bounded_lookback_not_origin \
    find=$'        _ => format!(\n            "{}:{}",\n            now_unix.saturating_sub(COLD_START_LOOKBACK.as_secs()),\n            "0".repeat(64)\n        ),' \
    replace=$'        _ => "head".to_string(),'

mutation "M2  a full channel drops instead of parking" \
    file=crates/doorbell-rt/src/wake_brain.rs \
    expect=a_replay_larger_than_the_channel_neither_drops_nor_ends_the_consumer \
    find='Err(tokio::sync::mpsc::error::TrySendError::Full(m)) => msg = m,' \
    replace='Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => return self.condemn_and_drop(false),'
