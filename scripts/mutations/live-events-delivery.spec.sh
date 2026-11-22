# Sourced by scripts/verify-mutations.sh.
# bd-znczi: the gated live events-delivery witness must stay wired into the
# e2e runner and every one of its tests must stay #[ignore]d.

CARGO_ARGS=(-p flywheel --test live_events_delivery_contract)
TEST_FILTER=(
    e2e_live_five_runs_the_events_delivery_witness
    live_events_delivery_is_gated_and_every_test_is_ignored
)

mutation "G1  the e2e runner drops the events-delivery witness" \
    file=scripts/e2e-live-five.sh \
    expect=e2e_live_five_runs_the_events_delivery_witness \
    find='    tests/live_events_delivery.rs' \
    replace='    : # bd-znczi witness dropped'

mutation "G2  the live witness loses its ignore discipline" \
    file=tests/live_events_delivery.rs \
    expect=live_events_delivery_is_gated_and_every_test_is_ignored \
    find=$'#[test]\n#[ignore = "gated live: FLYWHEEL_LIVE_E2E=1, run only via scripts/e2e-live-five.sh"]' \
    replace=$'#[ignore = "gated live: FLYWHEEL_LIVE_E2E=1, run only via scripts/e2e-live-five.sh"]\n#[test]'
