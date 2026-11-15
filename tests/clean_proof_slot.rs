use std::process::Command;

#[test]
fn clean_proof_slot_serializes_and_recovers() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("bash")
        .arg(format!("{root}/scripts/test-clean-proof-slot.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-clean-proof-slot.sh");
    assert!(status.success(), "scripts/test-clean-proof-slot.sh failed");
}
