# Sourced by scripts/verify-mutations.sh.
# bd-0uzrq.2: the idle edge rings by itself, once, and a second idle with no
# new mail rings nothing. Re-ring on an empty drain and the second idle pages a
# pane that has nothing to deliver.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(an_idle_edge_rings_exactly_once_and_a_second_idle_rings_nothing)

mutation "I1  an empty drain re-rings instead of re-reading the inbox" \
    file=crates/doorbell-core/src/lib.rs \
    expect=an_idle_edge_rings_exactly_once_and_a_second_idle_rings_nothing \
    find='                    actions.push(DoorbellAction::FetchInbox);' \
    replace='                    actions.push(DoorbellAction::Prompt { pin: pin.clone(), line: "re-read".into() });'
