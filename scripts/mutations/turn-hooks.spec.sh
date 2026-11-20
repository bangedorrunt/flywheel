# Sourced by scripts/verify-mutations.sh.
# bd-b2kaa: the continuation cap, turn-state precedence and urgent throttle.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    stop_continues_at_most_three_times
    a_fresh_report_wins_but_a_stale_one_does_not
    urgent_check_is_throttled_to_15_seconds
)

mutation "T1  Stop continues without the 3-time cap" \
    file=src/turn_state.rs \
    expect=stop_continues_at_most_three_times \
    find='pub const MAX_CONTINUATIONS: u32 = 3;' \
    replace='pub const MAX_CONTINUATIONS: u32 = 4;'

mutation "T2  hook turn reports never override the screen status" \
    file=src/turn_state.rs \
    expect=a_fresh_report_wins_but_a_stale_one_does_not \
    find='    match turn {
        Some(t) if now_us.saturating_sub(t.at_us) <= FRESH_SECS * 1_000_000 => {' \
    replace='    match None::<&TurnReport> {
        Some(t) if now_us.saturating_sub(t.at_us) <= FRESH_SECS * 1_000_000 => {'

mutation "T3  PostToolUse checks urgent mail on every tool call" \
    file=src/turn_state.rs \
    expect=urgent_check_is_throttled_to_15_seconds \
    find='pub const URGENT_CHECK_SECS: i64 = 15;' \
    replace='pub const URGENT_CHECK_SECS: i64 = 0;'
