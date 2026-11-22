//! bd-9idgs: verify-formal discovers models, cfgs and traces from file names.

use std::process::Command;

#[test]
fn formal_discovery_fixture() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("bash")
        .arg(format!("{root}/scripts/test-verify-formal-discovery.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-verify-formal-discovery.sh");
    assert!(status.success(), "formal discovery fixture failed");
}
