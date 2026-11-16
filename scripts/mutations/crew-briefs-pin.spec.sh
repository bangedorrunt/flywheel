# Sourced by scripts/verify-mutations.sh.
# `crew inbox --pin` narrows the open list to one crew (bd-0uzrq.4 slice 2).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(crew_briefs)

mutation "P1  the pin filter is ignored" \
    file=src/crew_briefs.rs \
    expect=the_open_list_filters_to_one_pin \
    find='            .filter(|b| pin.is_none_or(|p| b.pin == p))' \
    replace='            .filter(|_b| true)'
