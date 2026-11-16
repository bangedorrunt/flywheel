# Sourced by scripts/verify-mutations.sh.
# The pane input-line guard (bd-0uzrq.3): operator text in the box defers a
# wake, and opencode's echo of the line WE typed does not.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(pane_screen)

mutation "G1  operator text reads as an empty box" \
    file=src/pane_screen.rs \
    expect=every_captured_screen_classifies \
    find='    InputLine::OperatorText
}' \
    replace='    InputLine::Empty
}'

mutation "G2  the harness echo of our own line reads as operator text" \
    file=src/pane_screen.rs \
    expect=every_captured_screen_classifies \
    find='        || last_typed.is_some_and(|typed| !typed.is_empty() && content == typed.trim())
' \
    replace='        || false
'

mutation "G3  ring_wake ignores the captured input line" \
    file=src/inbox_ports.rs \
    expect=pane_screen_defers_a_typed_box_and_types_nothing \
    find='    if !crate::pane_screen::may_inject(input) {' \
    replace='    if false {'

mutation "G4  the crew prompt types over a typed box" \
    file=src/cmd/crew.rs \
    expect=pane_screen_crew_prompt_defers_on_a_typed_box \
    find='    crate::pane_read::defer_if_operator_typing(reader, harness, target, project)?;' \
    replace='    let _ = (reader, harness, target, project);'

mutation "G5  the send door stops asking the guard" \
    file=src/cmd/send.rs \
    expect=pane_screen_every_prompt_door_asks_the_input_guard \
    find='crate::pane_read::defer_if_operator_typing(
                &crate::pane_read::PlaneReader(&*backend),
                &crate::pane_read::harness_of(project, &target),
                &target,
                project,
            )
            .map_err(anyhow::Error::msg)?;' \
    replace='// mutation: the send door no longer asks the operator-typing guard'
