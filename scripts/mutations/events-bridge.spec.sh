# Sourced by scripts/verify-mutations.sh.
# bd-si4b5: the pure SEP-3415 client decisions that must fail if changed.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    notice_never_carries_the_body
    dedupe_suppresses_repeated_event_ids
    dedupe_evicts_oldest_not_the_whole_set
    gap_delivers_events_and_advances_cursor_with_gap_hint
    truncated_page_also_advances_and_emits_gap_hint
    emits_before_journal_cursor
    heartbeat_dead_after_three_intervals
    initialize_capability_is_under_experimental
    initialize_echoes_old_version_and_falls_back_for_modern
    notice_rpc_schema_is_string_content_plus_meta
)

mutation "E1  the event body leaks into the mapping" \
    file=src/events_bridge.rs \
    expect=notice_never_carries_the_body \
    find='        hint: HINT.to_string(),
        event_id: event.id.clone(),
        event_name: event.kind.clone(),' \
    replace='        hint: format!("{} {}", HINT, event.payload),
        event_id: event.id.clone(),
        event_name: event.kind.clone(),'

mutation "E2  event ids are not deduped" \
    file=src/events_bridge.rs \
    expect=dedupe_suppresses_repeated_event_ids \
    find='        if self.seen.contains(id) {
            return false;
        }' \
    replace='        if false && self.seen.contains(id) {
            return false;
        }'

mutation "E3  gap page does NOT advance to the fresh cursor" \
    file=src/events_bridge.rs \
    expect=gap_delivers_events_and_advances_cursor_with_gap_hint \
    find='    if !page.next_cursor.is_empty() {
        *cursor = page.next_cursor.clone();
        cmds.push(BridgeCmd::JournalCursor(page.next_cursor.clone()));
    }' \
    replace='    if !page.next_cursor.is_empty() && !page.needs_resync() {
        *cursor = page.next_cursor.clone();
        cmds.push(BridgeCmd::JournalCursor(page.next_cursor.clone()));
    }'

mutation "E4  heartbeat liveness drops to one interval" \
    file=src/events_bridge.rs \
    expect=heartbeat_dead_after_three_intervals \
    find='    now_ms.saturating_sub(last_hb_ms) > interval_ms.saturating_mul(3)' \
    replace='    now_ms.saturating_sub(last_hb_ms) > interval_ms'

mutation "E5  claude/channel capability moves to top level" \
    file=src/events_bridge.rs \
    expect=initialize_capability_is_under_experimental \
    find='        "capabilities": { "experimental": { CHANNEL_CAPABILITY: {} } },' \
    replace='        "capabilities": { CHANNEL_CAPABILITY: {} },'

mutation "E6  journal cursor before emit (loses wake on crash)" \
    file=src/events_bridge.rs \
    expect=emits_before_journal_cursor \
    find='        cmds.push(BridgeCmd::JournalCursor(page.next_cursor.clone()));' \
    replace='        cmds.insert(0, BridgeCmd::JournalCursor(page.next_cursor.clone()));'

mutation "E7  notification content becomes an array again" \
    file=src/events_bridge.rs \
    expect=notice_rpc_schema_is_string_content_plus_meta \
    find='            "content": notice.hint,' \
    replace='            "content": [{ "type": "text", "text": notice.hint }],'

mutation "E8  protocol version never echoes an old client" \
    file=src/events_bridge.rs \
    expect=initialize_echoes_old_version_and_falls_back_for_modern \
    find='        Some(v) if !v.is_empty() && v < MODERN_REVISION => v.to_string(),' \
    replace='        Some(_) if false => String::new(),'
