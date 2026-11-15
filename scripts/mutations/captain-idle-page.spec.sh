# Sourced by scripts/verify-mutations.sh.
#
# bd-iscat: the captain watch pages when a crew pane goes working -> idle
# holding unread captain mail (or an unrung deferral). The fold is pure, so each
# decision it makes has its own mutation.
#
#   scripts/verify-mutations.sh scripts/mutations/captain-idle-page.spec.sh --cargo mbx
#
# What this does not prove: the edge that builds the sample (registry -> status,
# inbox -> subjects, deferral file) — the live leg drives that end to end.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(cmd::captain::tests)

CAPTAIN=src/cmd/captain.rs
PAGE='            if self.working.contains(&row.pin) && !row.unread.is_empty() {'

mutation "C1  any idle-with-unread pages, transition or not" \
    file=$CAPTAIN \
    expect=idle_page_fires_once_per_transition \
    find="$PAGE" \
    replace='            if !row.unread.is_empty() {'

mutation "C2  an idle pin with an empty inbox still pages" \
    file=$CAPTAIN \
    expect=idle_page_never_fires_for_an_empty_inbox \
    find="$PAGE" \
    replace='            if self.working.contains(&row.pin) {'

mutation "C3  a queued deferral is not a brief" \
    file=$CAPTAIN \
    expect=a_queued_deferral_is_an_unread_brief \
    find='    if deferred {' \
    replace='    if false {'

mutation "C4  a system notice counts as a brief" \
    file=$CAPTAIN \
    expect=captain_subjects_skip_crew_senders \
    find='        .filter(|row| !crate::mail_wake::is_system_notice_value(row))' \
    replace='        .filter(|_row| true)'

mutation "C5  a crew peer's unread counts as captain mail" \
    file=$CAPTAIN \
    expect=captain_subjects_skip_crew_senders \
    find='    !crew.contains(sender)' \
    replace='    true || !crew.contains(sender)'
