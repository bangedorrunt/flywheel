# Sourced by scripts/verify-mutations.sh.
# bd-bkakk: the job-plane tripwire must recognise an @mention, not any '@'.
# Each mutant restores a false positive from captain mail quoting code.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(plane_route_mention_boundary_table)

mutation "M1  any @ character is a job mention" \
    file=src/plane_route.rs \
    expect=plane_route_mention_boundary_table \
    find='    mention_pin(body).is_some() && has_p_tag' \
    replace='    body.contains('\''@'\'') && has_p_tag'

mutation "M2  the word boundary around a pin is ignored" \
    file=src/plane_route.rs \
    expect=plane_route_mention_boundary_table \
    find='        if before.is_some_and(|c| {' \
    replace='        if false && before.is_some_and(|c| {'

mutation "M3  a lowercase token counts as a pin" \
    file=src/plane_route.rs \
    expect=plane_route_mention_boundary_table \
    find='        if !pin.as_bytes()[0].is_ascii_uppercase() {' \
    replace='        if false {'
