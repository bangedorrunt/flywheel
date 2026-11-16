# Sourced by scripts/verify-mutations.sh.
# bd-0uzrq.25: a short identical tail is a think, not a loop, and the loop
# verdict names the pane it accuses.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    a_thinking_pane_is_not_looping_until_the_run_outlives_a_turn
    activity_watch_seeds_the_clock_and_counts_only_busy_tails
)

mutation "M1  a think pages as a loop again" \
    file=src/cmd/captain.rs \
    expect=a_thinking_pane_is_not_looping_until_the_run_outlives_a_turn \
    find='if p.loop_repeats >= LOOP_REPEATS && p.loop_repeat_secs >= p.loop_min_secs {' \
    replace='if p.loop_repeats >= LOOP_REPEATS {'

mutation "M2  the loop forgets which pane it accuses" \
    file=src/cmd/captain.rs \
    expect=activity_watch_seeds_the_clock_and_counts_only_busy_tails \
    find='loop_pin: longest.cloned(),' \
    replace='loop_pin: None,'
