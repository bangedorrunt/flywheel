# Sourced by scripts/verify-mutations.sh.
#
# bd-0uzrq.8: a wake that cannot be typed is QUEUED, and the queue must never
# lose or hide one. The live incident: a captain brief and a peer request
# arrived in one fetch for a working pane; the brief vanished with no audit row.
#
#   scripts/verify-mutations.sh scripts/mutations/doorbell-merge.spec.sh --cargo mbx
#
# What this does not prove: the drain plumbing in the live watcher (PlaneStatus
# emission) and the workspace-side hold (doorbell-held.spec.sh).

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(tests::)

CORE=crates/doorbell-core/src/lib.rs

mutation "W1  routine mail is dropped at parse again" \
    file=$CORE \
    expect=a_peer_wake_never_hides_a_captain_brief \
    find='        EventClass::PlanePMail
    };' \
    replace='        return None;
    };'

mutation "W2  the composed ring names only its head" \
    file=$CORE \
    expect=a_peer_wake_never_hides_a_captain_brief \
    find='    format!("{head} + {}. {door}", rest.join(" + "))' \
    replace='    format!("{head}. {door}")'

mutation "W3  a peer line outranks the captain brief" \
    file=$CORE \
    expect=a_peer_wake_never_hides_a_captain_brief \
    find='    by_kind.sort_by_key(|w| std::cmp::Reverse(w.kind));' \
    replace='    by_kind.sort_by_key(|w| w.kind);'

mutation "W6  the two producers of one directive are not deduped" \
    file=$CORE \
    expect=one_directive_rings_once_through_two_producers \
    find='    if let Some(at) = duplicate {' \
    replace='    if let Some(at) = None::<usize> {'

mutation "W8  an identified brief folds onto another brief's text" \
    file=$CORE \
    expect=two_briefs_of_one_rendering_stay_two_wakes \
    find='            !w.key.is_empty() && w.key == key' \
    replace='            w.core == core'

mutation "W7  the command text makes two renderings of one message distinct" \
    file=$CORE \
    expect=the_wake_key_ignores_the_watermark_but_not_the_body \
    find='    line.split(". Read it with:")' \
    replace='    line.split("NEVER_MATCHES")'

mutation "W12  the corr fallback is dropped, so a bead correction rings beside its brief" \
    file=$CORE \
    expect=a_corr_supersedes_when_the_row_has_no_thread \
    find='    field("thread_id").or_else(|| field("corr"))' \
    replace='    field("thread_id")'

mutation "W11  a thread correction rings beside the brief it corrects" \
    file=$CORE \
    expect=a_thread_correction_supersedes_the_queued_brief \
    find='    if let Some(thread) = thread.as_deref().filter(|t| !t.is_empty())' \
    replace='    if let Some(thread) = None::<&str>'

mutation "W9  an already-rung message rings again on the idle re-read" \
    file=$CORE \
    expect=the_idle_edge_asks_for_the_inbox_when_the_queue_is_empty \
    find='    if !key.is_empty() && next.rung.contains(&key) {' \
    replace='    if false && next.rung.contains(&key) {'

mutation "W10  the idle edge does not ask for the inbox" \
    file=$CORE \
    expect=the_idle_edge_asks_for_the_inbox_when_the_queue_is_empty \
    find='                    actions.push(DoorbellAction::FetchInbox);' \
    replace='                    let _ = 0;'

mutation "W4  a lone system notice rides the ring" \
    file=$CORE \
    expect=a_system_notice_never_rings_and_says_so \
    find='                } else if deferred.iter().all(|w| w.kind == WakeKind::Notice) {' \
    replace='                } else if false {'
