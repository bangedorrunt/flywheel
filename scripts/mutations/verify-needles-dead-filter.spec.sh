# Sourced by scripts/verify-mutations.sh.
# bd-6setu.18: a TEST_FILTER naming no #[test] disarms its spec. The mutant
# accepts every filter again, so the fixture that demands a refusal goes RED.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_)

mutation "D1  every TEST_FILTER is accepted again" \
    file=scripts/verify-needles.sh \
    expect=proof_verify_needles_refuses_a_dead_test_filter \
    find='        live = any(seg in candidate for seg in segments for candidate in test_names)' \
    replace='        live = true'
