use flywheel_test_sandbox::TestSandbox;

#[test]
fn clean_proof_slot_serializes_and_recovers() {
    let root = env!("CARGO_MANIFEST_DIR");
    let sandbox = TestSandbox::new();
    let status = sandbox
        .command("/bin/bash")
        .env("PATH", sandbox.path_env())
        .arg(format!("{root}/scripts/test-clean-proof-slot.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-clean-proof-slot.sh");
    assert!(status.success(), "scripts/test-clean-proof-slot.sh failed");
}
