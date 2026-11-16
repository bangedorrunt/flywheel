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
