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
    find='    format!("{head} + {} — run: toron next", rest.join(" + "))' \
    replace='    format!("{head} — run: toron next")'

mutation "W3  a peer line outranks the captain brief" \
    file=$CORE \
    expect=a_peer_wake_never_hides_a_captain_brief \
    find='    by_kind.sort_by_key(|w| std::cmp::Reverse(w.kind));' \
    replace='    by_kind.sort_by_key(|w| w.kind);'

mutation "W4  a lone system notice rides the ring" \
    file=$CORE \
    expect=a_system_notice_never_rings_and_says_so \
    find='                } else if deferred.iter().all(|w| w.kind == WakeKind::Notice) {' \
    replace='                } else if false {'
