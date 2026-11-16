# Sourced by scripts/verify-mutations.sh.
# The reservation board model's fence (bd-0uzrq.12). The Rust refusal lives in
# toron (@51e27bde, pinned); these two rows prove the model that fences it is
# neither weakened nor quietly unregistered.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_reservation_board)

mutation "B1  a grant no longer needs an empty board" \
    file=.agents/skills/verify-protocol-tla/models/ReservationBoard.tla \
    expect=proof_reservation_board_model_fences_the_grant \
    find='[] OTHER                    -> v.status = "complete" /\ v.seen = {} /\ held = {}' \
    replace='[] OTHER                    -> v.status = "complete" /\ v.seen = {}'

mutation "B2  the truncated-read violation is unregistered" \
    file=scripts/verify-formal.sh \
    expect=proof_reservation_board_model_fences_the_grant \
    find='expect_violation ReservationBoard ReservationBoard.broken-truncated-grants NoGrantWithoutCompleteBoard' \
    replace='true  # mutation: the truncated-read violation is not checked'
