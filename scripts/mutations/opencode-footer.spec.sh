# Sourced by scripts/verify-mutations.sh.
# bd-g4xlr: a status footer drawn in the opencode input box is not typed text.
# Drop the structural footer rule and every wake to an idle pane defers with
# "operator is typing".

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(every_captured_screen_classifies)

mutation "F1  the opencode footer is read as typed text" \
    file=src/pane_screen.rs \
    expect=every_captured_screen_classifies \
    find='        "opencode" => marker_box(tail, "┃", &is_input_footer),' \
    replace='        "opencode" => marker_box(tail, "┃", &|_| false),'
