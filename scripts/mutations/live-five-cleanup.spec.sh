# Sourced by scripts/verify-mutations.sh.
# bd-833ya: the live five must deregister its throwaway project and drop its
# supervisor unit. Removing the cleanup call must turn the witness RED.

CARGO_ARGS=(-p flywheel --test e2e_live_dry_run)
TEST_FILTER=(the_live_five_dry_run_plans_without_creating_a_session)

mutation "L1  live five leaves its throwaway project registered" \
    file=scripts/e2e-live-five.sh \
    expect=the_live_five_dry_run_plans_without_creating_a_session \
    find=$'    if [ "$KEEP" = 0 ]; then\n        retire_throwaway_project "$PROJECT"\n    fi' \
    replace=$'    if [ "$KEEP" = 0 ]; then\n        : # mutant: no project cleanup\n    fi'
