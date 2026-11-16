# Sourced by scripts/verify-mutations.sh.
# Every quality-gate leg denies warnings (bd-0uzrq.4 finding): a bare `check`
# compiled a dead binding that the -D warnings build caught.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_every_gate_leg)

mutation "W1  a gate leg stops denying warnings" \
    file=scripts/verify-clippy.sh \
    expect=proof_every_gate_leg_denies_warnings \
    find='export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"' \
    replace='export RUSTFLAGS="${RUSTFLAGS:-}"'
