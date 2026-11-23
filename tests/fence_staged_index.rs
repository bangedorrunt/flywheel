//! bd-4z7mb: the commit-time gate. `/tmp` for the fixture, no cargo, no network:
//! the script below stages a caller whose callee stays dirty and asserts the
//! index fence refuses it, naming the build error, while the pair staged
//! together passes.

use flywheel_test_sandbox::TestSandbox;

#[test]
fn the_fence_refuses_a_staged_caller_without_its_callee() {
    let root = env!("CARGO_MANIFEST_DIR");
    let sandbox = TestSandbox::new();
    let real_home = std::env::var("HOME").unwrap_or_default();
    let status = sandbox
        .command("/bin/bash")
        .env("PATH", sandbox.path_env())
        .env("RUSTUP_HOME", format!("{real_home}/.rustup"))
        .env("CARGO_HOME", format!("{real_home}/.cargo"))
        .arg(format!("{root}/scripts/test-fence.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-fence.sh");
    assert!(
        status.success(),
        "the staging witness failed: the fence must prove the index, not the tree"
    );
}
