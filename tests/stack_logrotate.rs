//! bd-9zw9u: install must rewrite an invalid toron logrotate side plist.

use flywheel_test_sandbox::TestSandbox;

#[test]
fn stack_logrotate_rewrite_and_status() {
    let root = env!("CARGO_MANIFEST_DIR");
    let sandbox = TestSandbox::new();
    let status = sandbox
        .command("/bin/bash")
        .env("PATH", sandbox.path_env())
        .arg(format!("{root}/scripts/test-stack-logrotate.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-stack-logrotate.sh");
    assert!(status.success(), "stack logrotate fixture failed");
}
