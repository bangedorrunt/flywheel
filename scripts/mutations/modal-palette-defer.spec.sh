# Sourced by scripts/verify-mutations.sh.
# An open command palette defers even when the input box is empty.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(every_captured_screen_classifies)

mutation "P1  an open command palette is ignored" \
    file=src/pane_screen.rs \
    expect=every_captured_screen_classifies \
    find='    if harness.as_str() == "opencode" && opencode_palette_open(tail) {' \
    replace='    if false {'
