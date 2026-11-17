# Sourced by scripts/verify-mutations.sh.
# bd-8wdgn: a box holding the doorbell's OWN queued wake line is not operator
# text. Treat it as operator text again and the deferral accuses the operator of
# typing, which is the live failure this bead exists for.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_queued_wake_line_defers_with_its_own_message every_captured_screen_classifies)

mutation "W1  our own queued wake line is classified as operator text" \
    file=src/pane_screen.rs \
    expect=a_queued_wake_line_defers_with_its_own_message \
    find='    WAKE_PREFIXES.iter().any(|p| content.starts_with(p))' \
    replace='    let _ = content;
    false'
