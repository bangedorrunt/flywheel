# Sourced by scripts/verify-mutations.sh.
# bd-jhz8x.3 (ADR-0021 D4.4): the `.done` sidecar is derived from a PASSING
# witness, after the exit. Write it before the gate decides and a refused step
# leaves a marker that no witnessed exit produced.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_witnessed_exit_writes_the_done_sidecar_and_a_refusal_writes_none)

mutation "W1  the sidecar is written before the witness decides" \
    file=src/durable_body.rs \
    expect=a_witnessed_exit_writes_the_done_sidecar_and_a_refusal_writes_none \
    find='    match witness_gate(seams.witness.as_ref(), node, stage, &value) {' \
    replace='    write_done_sidecar(seams.sidecar_root.as_deref(), trigger, node);
    match witness_gate(seams.witness.as_ref(), node, stage, &value) {'
