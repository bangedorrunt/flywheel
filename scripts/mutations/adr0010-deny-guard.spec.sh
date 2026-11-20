# Sourced by scripts/verify-mutations.sh.
# bd-b2kaa follow-up: the narrowed ADR-0010 D13 guard must still fail on a
# real Claude permissions deny list (seeded in the test).

CARGO_ARGS=(--test adr0010_d13)
TEST_FILTER=(adr0010_d13_no_tracked_claude_only_deny_list)

mutation "D1  the deny-list shape guard never fires" \
    file=tests/adr0010_d13.rs \
    expect=adr0010_d13_no_tracked_claude_only_deny_list \
    find='            if window.contains("\"deny\":") || window.contains("\"deny_list\":") {' \
    replace='            if false {'
