# Sourced by scripts/verify-mutations.sh.
# bd-7szny: dedup must cut at chie's trap-matcher framing, not at the first
# backtick anywhere — code identifiers in the rule text would collide.

CARGO_ARGS=(--test memory_loop)
TEST_FILTER=(distinct_code_lessons_both_surface recalled_lessons_are_deduplicated)

mutation "D1  the core is cut at the first backtick instead of the trap boundary" \
    file=src/cmd/memory.rs \
    expect=distinct_code_lessons_both_surface \
    find='    let before_trap = &bare[..trap_boundary(bare)];' \
    replace="    let before_trap = &bare[..bare.find('\`').unwrap_or(trap_boundary(bare))];"
