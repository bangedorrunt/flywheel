use std::process::Command;

#[test]
fn stack_recover_script_cases() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("bash")
        .arg(format!("{root}/scripts/test-stack-recover.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-stack-recover.sh");
    assert!(status.success(), "scripts/test-stack-recover.sh failed");
}
