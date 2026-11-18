# Sourced by scripts/verify-mutations.sh.
# bd-oo7zi: the throwawayrex session label must be owned by the e2e project;
# a foreign label makes crew prompt answer "no flywheel swarm".

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(the_live_five_default_session_is_owned_by_its_project)

mutation "L1  the e2e session label is unrelated to its project" \
    file=scripts/e2e-live-five.sh \
    expect=the_live_five_default_session_is_owned_by_its_project \
    find='FLYWHEEL_LIVE_SESSION:-flywheel-$PROJECT-' \
    replace='FLYWHEEL_LIVE_SESSION:-flywheel-live-'
