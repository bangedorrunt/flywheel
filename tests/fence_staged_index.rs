//! bd-4z7mb: the commit-time gate. `/tmp` for the fixture, no cargo, no network:
//! the script below stages a caller whose callee stays dirty and asserts the
//! index fence refuses it, naming the build error, while the pair staged
//! together passes.

use std::process::Command;

#[test]
fn the_fence_refuses_a_staged_caller_without_its_callee() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("bash")
        .arg(format!("{root}/scripts/test-fence.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-fence.sh");
    assert!(
        status.success(),
        "the staging witness failed: the fence must prove the index, not the tree"
    );
}
