# Sourced by scripts/verify-mutations.sh.
# bd-m97cj: dropping the expect/TEST_FILTER selection check lets a mutant read
# VACUOUS and prove nothing. The fixture then stops refusing the bad spec.

CARGO_ARGS=(--test proof_steps)
TEST_FILTER=(proof_verify_needles_refuses_an_unselected_expect)

mutation "N1  the expect selection check never fires" \
    file=scripts/verify-needles.sh \
    expect=proof_verify_needles_refuses_an_unselected_expect \
    find='    if expect and green == "0":
        unselected_expects.append((spec, mid, expect))' \
    replace='    if False:
        unselected_expects.append((spec, mid, expect))'
