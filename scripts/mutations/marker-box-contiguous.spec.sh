# Sourced by scripts/verify-mutations.sh.
# The box is the contiguous marker run nearest the bottom.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(every_captured_screen_classifies)

mutation "M1  marker_box walks past non-marker lines into history" \
    file=src/pane_screen.rs \
    expect=every_captured_screen_classifies \
    find='            if saw_box && !trimmed.is_empty() {
                break;
            }' \
    replace='            let _ = (saw_box, trimmed);'
