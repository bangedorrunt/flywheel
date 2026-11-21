# Sourced by scripts/verify-mutations.sh.
# bd-7ky1v: stream mode emits one line per event and never exits.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    stream_emits_one_line_per_event_and_never_exits
    one_shot_exits_on_the_first_event_unchanged
)

mutation "W1  stream mode emits no lines" \
    file=src/elm/captain_watch_statechart.rs \
    expect=stream_emits_one_line_per_event_and_never_exits \
    find='                    if ctx.stream {' \
    replace='                    if false && ctx.stream {'

mutation "W2  duplicate events are re-emitted" \
    file=src/elm/captain_watch_statechart.rs \
    expect=stream_emits_one_line_per_event_and_never_exits \
    find='                        if ctx.emitted.insert(format!("{kind}:{id}")) {' \
    replace='                        if true { let _ = ctx.emitted.insert(format!("{kind}:{id}"));'
