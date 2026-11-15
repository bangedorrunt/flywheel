# Sourced by scripts/verify-mutations.sh.
#
# bd-0uzrq.16: a typed wake can be written and submitted with nothing behind it
# (a reset opencode on its welcome screen swallowed the paste live, 2026-11-15
# 13:47Z) and the writer must see the silence, not report a wake that never
# rang. Every arm below exists to be taken away.
#
#   scripts/verify-mutations.sh scripts/mutations/crew-prompt-landing.spec.sh --cargo mbx
#
# What this does not prove: the live landing itself (`agent_wait`/`agent_get`
# against a real pane). The bead's live leg drives that end: reset a scratch
# opencode to its welcome screen, crew prompt it, and a turn starts.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(cmd::crew::tests)

CREW=src/cmd/crew.rs

mutation "L1  a silent pane counts as a landed wake (trust the write)" \
    file=$CREW \
    expect=a_welcome_screen_that_swallows_the_paste_is_a_typed_failure \
    find='        Ok(false) => Err(wake_not_landed(target, bound)),' \
    replace='        Ok(false) => Ok(done),'

mutation "L2  the landing check is skipped once the line is typed" \
    file=$CREW \
    expect=a_welcome_screen_that_swallows_the_paste_is_a_typed_failure \
    find='    match wait_turn(target, bound, before, probe) {' \
    replace='    match Ok::<bool, String>(true) {'

mutation "L5  a screen that never moved still counts as a turn" \
    file=$CREW \
    expect=a_stale_working_record_cannot_confirm_a_silent_wake \
    find='        let moved = before.is_none_or(|before| screen_digest(&screen) != before);' \
    replace='        let moved = true;'

mutation "L6  a moved screen alone counts as a landing" \
    file=$CREW \
    expect=a_moved_screen_without_a_turn_is_not_a_landing \
    find='        if in_turn && moved {' \
    replace='        if moved {'

mutation "L3  an unconsumed wake fails the prompt instead of deferring" \
    file=$CREW \
    expect=an_unconsumed_wake_defers_to_the_dispatcher_with_its_reason \
    find='    wake_is_busy(err) || err.contains("wake line did not land")' \
    replace='    wake_is_busy(err)'

mutation "L4  the journal drops why the ring was deferred" \
    file=$CREW \
    expect=an_unconsumed_wake_defers_to_the_dispatcher_with_its_reason \
    find='            Ok((QUEUED_WAKE.to_string(), Some(err)))' \
    replace='            Ok((QUEUED_WAKE.to_string(), None))'
