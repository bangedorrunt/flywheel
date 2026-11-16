# Sourced by scripts/verify-mutations.sh.
# bd-jp9z1.4: the sandbox Drop must catch a reached supervisor, and the lint
# must refuse a spawn that bypasses the helper.

CARGO_ARGS=(--test sandbox_contract)
TEST_FILTER=(
    drop_panics_when_a_supervisor_shim_was_reached
    lint_refuses_a_test_that_spawns_the_binary_without_the_helper
)

mutation "M1  Drop stops asserting the deny-shim log" \
    file=crates/test-sandbox/src/lib.rs \
    expect=drop_panics_when_a_supervisor_shim_was_reached \
    find='self.allow_supervisor.load(Ordering::SeqCst) || log.is_empty(),' \
    replace='true,'

mutation "M2  the lint stops refusing an offender" \
    file=scripts/check-test-sandbox.sh \
    expect=lint_refuses_a_test_that_spawns_the_binary_without_the_helper \
    find=$'    echo "test-sandbox: $file spawns a built binary without src/test_sandbox.rs" >&2\n    return 0' \
    replace=$'    return 1'
