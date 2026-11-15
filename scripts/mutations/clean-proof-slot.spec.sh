# Sourced by scripts/verify-mutations.sh.
# bd-jp9z1.1: clean-proof must serialize with one machine-wide slot.

CARGO_ARGS=(-p flywheel --test clean_proof_slot)
TEST_FILTER=(clean_proof_slot_serializes_and_recovers)

mutation "S1  slot acquisition dropped" \
    file=scripts/clean-proof.sh \
    expect=clean_proof_slot_serializes_and_recovers \
    find=$'if [ "${CLEAN_PROOF_SLOTTED:-}" != 1 ]; then\n  export CLEAN_PROOF_SLOTTED=1\n' \
    replace=$'if false; then\n  export CLEAN_PROOF_SLOTTED=1\n'
