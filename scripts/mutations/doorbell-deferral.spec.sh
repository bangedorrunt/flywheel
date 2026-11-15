# Sourced by scripts/verify-mutations.sh.
# A busy pane DEFERS its wake; the sink never parks for it (bd-9mjla).

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(busy_refusal_defers_and_never_parks_the_sink)

mutation "D1  a busy refusal parks the sink instead of deferring" \
    file=crates/doorbell-rt/src/sinks.rs \
    expect=busy_refusal_defers_and_never_parks_the_sink \
    find=$'            if matches!(e, RtError::PaneBusy(_)) {\n                let _ = degraded_tx\n                    .send(BrainMsg::PromptRefused {\n                        pin: pin.clone(),\n                        line: line.clone(),\n                    })\n                    .await;\n                continue;\n            }\n' \
    replace=$'            // mutation: busy-deferral arm removed\n'
