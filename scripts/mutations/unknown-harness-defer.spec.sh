# Sourced by scripts/verify-mutations.sh.
# bd-e4gal: a profileless spawn writes no `harness` to its registry row, so the
# guard is handed "". Falling back to input_line_for("") returns Unknown, which
# may_inject treats as safe -- and a wake lands on the operator's half-typed
# line (the real live P1). Structurally classifying the box is the fix.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(an_unknown_harness_defers_when_the_box_holds_operator_text)

mutation "E1  an unknown harness is treated as injectable again" \
    file=src/pane_read.rs \
    expect=an_unknown_harness_defers_when_the_box_holds_operator_text \
    find='        input_line_for_unknown_harness(&tail, last_typed)' \
    replace='        input_line_for("", &tail, last_typed)'
