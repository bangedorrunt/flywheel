# Sourced by scripts/verify-mutations.sh.
# bd-0uzrq.2: the idle edge rings ONCE per drained window, coalescing every
# queued class. Ring per queued wake instead and a pane with two classes gets
# two prompts for one idle edge.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(an_idle_edge_rings_exactly_once_and_a_second_idle_rings_nothing)

mutation "I1  the drain rings once per queued wake instead of coalescing" \
    file=crates/doorbell-core/src/lib.rs \
    expect=an_idle_edge_rings_exactly_once_and_a_second_idle_rings_nothing \
    find='                    for w in &deferred {
                        if !w.key.is_empty() {
                            next.rung.insert(w.key.clone());
                        }
                    }' \
    replace='                    for w in &deferred {
                        if !w.key.is_empty() {
                            next.rung.insert(w.key.clone());
                            actions.push(DoorbellAction::Prompt {
                                pin: pin.clone(),
                                line: w.line.clone(),
                            });
                        }
                    }'
