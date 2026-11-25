# Sourced by scripts/verify-mutations.sh.
# bd-ihiqo.3: the work unit closes only on a green verify, and an answered
# collab ask never counts as an open wait. Dropping either fence must turn
# the matching test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(one_message_log_per_close_route_plus_handoff an_unverified_close_does_not_fire)

mutation "W1  verify never marks green" \
    file=src/elm/work_unit_statechart.rs \
    expect=one_message_log_per_close_route_plus_handoff \
    find='                        ctx.verified = true;' \
    replace='                        ctx.verified = false;'
