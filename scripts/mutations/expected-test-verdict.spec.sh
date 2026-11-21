# Sourced by scripts/verify-mutations.sh. bd-feu67: credit RED only when the
# expected test itself failed, never on a substring match of a passing line.

CARGO_ARGS=(--test proof_steps)
TEST_FILTER=(proof_verify_mutations_credits_only_the_named_test)

mutation "V1  RED reverts to a substring match" \
    file=scripts/verify-mutations.sh \
    expect=proof_verify_mutations_credits_only_the_named_test \
    find='            if expected_test_failed "$out" "$expect"; then' \
    replace='            if [[ "$out" == *"$expect"* ]]; then'
