# Sourced by scripts/verify-mutations.sh.
# bd-si4b5: the pure SEP-3415 client decisions that must fail if changed.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    notice_never_carries_the_body
    dedupe_suppresses_repeated_event_ids
    gap_or_truncated_emits_nothing_and_does_not_advance_cursor
    heartbeat_dead_after_three_intervals
    initialize_declares_channel_and_notice_rpc_never_carries_body
)

mutation "E1  the event body leaks into the mapping" \
    file=src/events_bridge.rs \
    expect=notice_never_carries_the_body \
    find='    Some(ChannelNotice {
        hint: HINT.to_string(),
        event_id: event.id.clone(),
    })' \
    replace='    Some(ChannelNotice {
        hint: format!("{} {}", HINT, event.payload),
        event_id: event.id.clone(),
    })'

mutation "E2  event ids are not deduped" \
    file=src/events_bridge.rs \
    expect=dedupe_suppresses_repeated_event_ids \
    find='    pub fn is_new(&mut self, id: &str) -> bool {
        if self.seen.contains(id) {
            return false;
        }' \
    replace='    pub fn is_new(&mut self, id: &str) -> bool {
        if false && self.seen.contains(id) {
            return false;
        }'

mutation "E3  gap/truncated pages emit partial results" \
    file=src/events_bridge.rs \
    expect=gap_or_truncated_emits_nothing_and_does_not_advance_cursor \
    find='    pub fn needs_resync(&self) -> bool {
        self.gap || self.truncated
    }' \
    replace='    pub fn needs_resync(&self) -> bool {
        false
    }'

mutation "E4  heartbeat liveness drops to one interval" \
    file=src/events_bridge.rs \
    expect=heartbeat_dead_after_three_intervals \
    find='    now_ms.saturating_sub(last_hb_ms) > interval_ms.saturating_mul(3)' \
    replace='    now_ms.saturating_sub(last_hb_ms) > interval_ms'

mutation "E5  claude/channel capability not declared" \
    file=src/events_bridge.rs \
    expect=initialize_declares_channel_and_notice_rpc_never_carries_body \
    find='        "capabilities": { CHANNEL_CAPABILITY: {} },' \
    replace='        "capabilities": {},'
