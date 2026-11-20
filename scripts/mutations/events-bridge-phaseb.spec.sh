# Sourced by scripts/verify-mutations.sh.
# bd-si4b5 Phase B: the real transport + burst coalescing must fail if changed.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    a_burst_of_wake_events_is_one_hint_with_a_count
    stream_frames_are_routed_by_subscription_and_decoded
    a_stream_failure_falls_back_to_poll
    a_gap_stream_frame_is_a_resync_page
)

mutation "P1  a burst emits one hint per event again" \
    file=src/events_bridge.rs \
    expect=a_burst_of_wake_events_is_one_hint_with_a_count \
    find='    if let Some(notice) = coalesce(burst) {
        cmds.push(BridgeCmd::Emit(notice));
    }' \
    replace='    for notice in burst {
        cmds.push(BridgeCmd::Emit(notice));
    }'

mutation "P2  stream frames ignore the subscription routing" \
    file=src/events_bridge.rs \
    expect=stream_frames_are_routed_by_subscription_and_decoded \
    find='    if routed != subscription {
        return None;
    }' \
    replace='    if false && routed != subscription {
        return None;
    }'

mutation "P3  a failed stream never falls back to poll" \
    file=src/events_client.rs \
    expect=a_stream_failure_falls_back_to_poll \
    find='                Ok(page_from_poll(&result))' \
    replace='                Err("poll fallback disabled".to_string())'

mutation "P4  a stream gap frame is dropped" \
    file=src/events_client.rs \
    expect=a_gap_stream_frame_is_a_resync_page \
    find='                    "notifications/events/gap" => gap = true,' \
    replace='                    "notifications/events/gap" => {}'
