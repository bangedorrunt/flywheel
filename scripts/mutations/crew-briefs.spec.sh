# Sourced by scripts/verify-mutations.sh.
# A crew brief is closed by its OWN id (bd-0uzrq.4). Dropping the id match is
# the live failure: a done reply that closes nothing leaves the brief ageing in
# the open list forever.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(crew_briefs)

mutation "B1  a reply closes whatever brief it finds" \
    file=src/crew_briefs.rs \
    expect=a_replayed_reply_emits_nothing \
    find='                if brief.closed_ts_us.is_some() {' \
    replace='                if false {'

mutation "B2  the reply never finds its brief" \
    file=src/crew_briefs.rs \
    expect=a_done_reply_closes_the_brief_and_wakes_the_captain_once \
    find='let Some(brief) = self.briefs.iter_mut().find(|b| b.id == id) else {' \
    replace='let Some(brief) = self.briefs.iter_mut().find(|b| b.id == "never") else {'
