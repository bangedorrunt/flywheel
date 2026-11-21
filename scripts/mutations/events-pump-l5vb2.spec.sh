# Sourced by scripts/verify-mutations.sh.
# bd-l5vb2 U8: the four load-bearing lines of the toron events pump.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    the_fetch_journal_lands_before_the_rpc_invoke
    a_poll_page_after_a_stream_failure_still_emits_one_hint
    flag_off_spawns_no_pump
    the_same_bus_event_id_rings_once_across_wake_sock_and_events
    a_gap_collapses_to_one_digest_msg
)

mutation "J1  the fetch intent is journaled AFTER the RPC invoke" \
    file=src/events_source.rs \
    expect=the_fetch_journal_lands_before_the_rpc_invoke \
    find='    journal.row("fetch", None);
    let page = client.fetch(cursor)?;' \
    replace='    let page = client.fetch(cursor)?;
    journal.row("fetch", None);'

mutation "P1  a failed stream never falls back to poll" \
    file=src/events_client.rs \
    expect=a_poll_page_after_a_stream_failure_still_emits_one_hint \
    find='                Ok(page_from_poll(&result))' \
    replace='                Err("poll fallback disabled".to_string())'

mutation "F1  the flag-off guard is removed so a seat is spawned anyway" \
    file=src/events_source.rs \
    expect=flag_off_spawns_no_pump \
    find='    if !enabled {
        return Vec::new();
    }' \
    replace='    if false {
        return Vec::new();
    }'

mutation "G1  the shared bus-id deduper is bypassed so a mail rings twice" \
    file=src/events_source.rs \
    expect=the_same_bus_event_id_rings_once_across_wake_sock_and_events \
    find='    let cmds = events_bridge::apply_page(deduper, cursor, page);' \
    replace='    let mut fresh = events_bridge::Deduper::new(1024);
    let cmds = events_bridge::apply_page(&mut fresh, cursor, page);'

mutation "D1  a gap replay is emitted as a mail hint instead of one digest" \
    file=src/events_source.rs \
    expect=a_gap_collapses_to_one_digest_msg \
    find='                Some(BrainMsg::EventsGap {
                    pin: pin.to_string(),
                })' \
    replace='                Some(BrainMsg::EventsHint {
                    event_id: "gap".into(), pin: pin.to_string(), line: mail_line(),
                })'
