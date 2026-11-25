# Sourced by scripts/verify-mutations.sh.
# Seat resume: each harness reopens its own way, Claude keeps its plugin, gemini is deprecated.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    each_supported_harness_reopens_its_own_way
    claude_resume_keeps_the_seat_plugin
    gemini_is_deprecated_for_antigravity
    claude_session_is_a_uuid_passed_with_session_id
    arrive_moves_a_seat_only_when_its_row_names_another_pane
    prune_keeps_dropped_pinned_rows_newest_per_pin
    a_pruned_seat_restores_into_the_live_registry_and_retire_forgets_it
    resume_all_brings_back_only_seats_that_can_reopen
)

mutation "R1  agy resume keeps the hook's harness prefix" \
    file=src/seat_resume.rs \
    expect=each_supported_harness_reopens_its_own_way \
    find='            Ok(vec![resume.to_string(), bare_id(id).to_string()])' \
    replace='            Ok(vec![resume.to_string(), id.to_string()])'

mutation "R2  pi continues nothing" \
    file=src/harness_registry.rs \
    expect=each_supported_harness_reopens_its_own_way \
    find='        session: SessionResume::SeatDirContinue { flag: "--continue" },' \
    replace='        session: SessionResume::SeatDirContinue { flag: "--resume" },'

mutation "R3  resume drops the Claude plugin" \
    file=src/seat_resume.rs \
    expect=claude_resume_keeps_the_seat_plugin \
    find='    args.extend(crate::cmd::spawn::claude_plugin_args(harness));' \
    replace='    let _ = crate::cmd::spawn::claude_plugin_args(harness);'

mutation "R4  gemini is no longer deprecated" \
    file=src/harness_registry.rs \
    expect=gemini_is_deprecated_for_antigravity \
    find='        deprecated_for: Some("antigravity"),' \
    replace='        deprecated_for: None,'

mutation "R5  claude spawn mints a non-uuid session" \
    file=src/cmd/spawn.rs \
    expect=claude_session_is_a_uuid_passed_with_session_id \
    find='        claude_session_id()' \
    replace='        opencode_session_id()'

mutation "R6  arrive leaves a moved seat on its old pane" \
    file=src/main.rs \
    expect=arrive_moves_a_seat_only_when_its_row_names_another_pane \
    find='    seat.filter(|(_, row)| row.pane_id != pane)' \
    replace='    seat.filter(|(_, row)| row.pane_id == pane)'

mutation "R7  the resumable list keeps every old row of a pin" \
    file=src/state.rs \
    expect=prune_keeps_dropped_pinned_rows_newest_per_pin \
    find='        archive.retain(|r| r.mail_identity.as_deref() != Some(pin));' \
    replace='        let _ = pin;'

mutation "R8  restore leaves the seat on the resumable list" \
    file=src/state.rs \
    expect=a_pruned_seat_restores_into_the_live_registry_and_retire_forgets_it \
    find='    let row = archive.remove(at);' \
    replace='    let row = archive[at].clone();'

mutation "R9  resume --all launches seats that cannot reopen" \
    file=src/seat_resume.rs \
    expect=resume_all_brings_back_only_seats_that_can_reopen \
    find='            Err(e) => skipped.push((pin, format!("{e:#}"))),' \
    replace='            Err(_) => ready.push(pin),'
