# Sourced by scripts/verify-mutations.sh.
# A crew pin may not install the fleet's binary or bounce its daemons
# (bd-0uzrq.6); the guard is the only thing refusing it.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(authority::tests::a_crew_pin_may_not_install_or_bounce_the_fleet)

mutation "G1  the fleet guard lets a crew pin through" \
    file=src/authority.rs \
    expect=a_crew_pin_may_not_install_or_bounce_the_fleet \
    find=$'    if operator {\n        return Ok(());\n    }\n' \
    replace=$'    if operator || true {\n        return Ok(());\n    }\n'
