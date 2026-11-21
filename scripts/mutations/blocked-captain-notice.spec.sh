# Sourced by scripts/verify-mutations.sh.
# bd-6qae0: a blocked-seat notice must reach the captain as a fact.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(blocked_notice_becomes_a_captain_fact)

mutation "B5  a blocked notice never becomes a captain fact" \
    file=src/doorbell.rs \
    expect=blocked_notice_becomes_a_captain_fact \
    find='    kind == "blocked_notice"' \
    replace='    false'
