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

# bd-yvnh6: a reintroduced bare spawn in commit_hook_guard must fail the
# check-test-sandbox.sh scan, not just the lint's self-test.
mutation "M3  commit_hook_guard reintroduces an unsandboxed spawn" \
    file=tests/commit_hook_guard.rs \
    expect=lint_refuses_a_test_that_spawns_the_binary_without_the_helper \
    find='use flywheel_test_sandbox::TestSandbox;' \
    replace='struct TestSandbox { dir: PathBuf }
impl TestSandbox {
    fn new() -> Self { Self { dir: std::env::temp_dir().join(format!("hook-{}", std::process::id())) } }
    fn root(&self) -> &Path { &self.dir }
    fn bin_dir(&self) -> PathBuf { self.dir.join("bin") }
    fn command(&self, p: &str) -> Command { Command::new(p) }
}'
