# Sourced by scripts/verify-mutations.sh.
# bd-q52vj: a test that resolves last_sent/state/registry paths must hold a
# hermetic guard; dropping it must turn the structural check RED.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(state_path_tests_hold_a_hermetic_guard)

mutation "S1  the corrupt-file test loses its state-home guard" \
    file=src/state.rs \
    expect=state_path_tests_hold_a_hermetic_guard \
    find='    fn load_corrupt_file_returns_empty() {
        let project = "__test_corrupt__";' \
    replace='    fn load_corrupt_file_returns_empty() {
        let project = "__test_corrupt__";
        return;'
