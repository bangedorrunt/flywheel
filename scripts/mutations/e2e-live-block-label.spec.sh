# Sourced by scripts/verify-mutations.sh.
# bd-oo7zi: flywheel resolves a rex pane by its BLOCK LABEL. Without the rename,
# `flywheel send --to PIN` cannot see the throwaway pane.

CARGO_ARGS=(--test live_doorbell_operator_guard)
TEST_FILTER=(the_harness_names_the_throwaway_block_after_the_pin)

mutation "B1  the harness never renames the throwaway block" \
    file=scripts/e2e-live-five.sh \
    expect=the_harness_names_the_throwaway_block_after_the_pin \
    find='block rename -s "$SESSION" "$BLOCK_ID" "$PIN"' \
    replace='block rename -s "$SESSION" "$BLOCK_ID" "not-the-pin"'
