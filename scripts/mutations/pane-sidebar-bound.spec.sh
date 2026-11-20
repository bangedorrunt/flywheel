# Sourced by scripts/verify-mutations.sh.
# bd-3p7pe: the input-box column bound is what keeps sidebar text out of the
# operator-typing detector. Drop it and the real capture reads OperatorText.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(every_captured_screen_classifies)

mutation "S1  the sidebar column bound is dropped" \
    file=src/pane_screen.rs \
    expect=every_captured_screen_classifies \
    find='    let right = sidebar_column(tail, marker);' \
    replace='    let right = sidebar_column(tail, marker).filter(|_| false);'
