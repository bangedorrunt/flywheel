# Sourced by scripts/verify-mutations.sh.
# bd-4z7mb follow-up: the commit gate must run CLIPPY, not check. A clippy-only
# error (a complex type) has to be refused at commit time; demote the gate back
# to `check` and it is silently accepted.

CARGO_ARGS=(--test fence_staged_index)
TEST_FILTER=(the_fence_refuses_a_staged_caller_without_its_callee)

mutation "C1  the gate runs check, not clippy" \
    file=scripts/fence.sh \
    expect=the_fence_refuses_a_staged_caller_without_its_callee \
    find='    $CARGO clippy --workspace --all-targets -- -D warnings' \
    replace='    $CARGO check --workspace --all-targets'
