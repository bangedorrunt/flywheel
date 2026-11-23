use flywheel_test_sandbox::TestSandbox;

#[test]
fn stack_recover_script_cases() {
    let root = env!("CARGO_MANIFEST_DIR");
    let sandbox = TestSandbox::new();
    let status = sandbox
        .command("/bin/bash")
        .env("PATH", sandbox.path_env())
        .arg(format!("{root}/scripts/test-stack-recover.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-stack-recover.sh");
    assert!(status.success(), "scripts/test-stack-recover.sh failed");
}
