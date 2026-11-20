# Sourced by scripts/verify-mutations.sh.
# bd-7szny: dedup must key on a normalized lesson core, not the raw chie
# rendering. Reverting to a raw compare surfaces the same lesson twice.

CARGO_ARGS=(--test memory_loop)
TEST_FILTER=(recalled_lessons_are_deduplicated)

mutation "D1  the normalized lesson core is not cut at the trap matcher" \
    file=src/cmd/memory.rs \
    expect=recalled_lessons_are_deduplicated \
    find="    let before_trap = bare.split('\`').next().unwrap_or(bare);" \
    replace='    let before_trap = bare;'
