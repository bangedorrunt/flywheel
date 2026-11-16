//! bd-gmrxv: the fence proves the commit's PINS, not the tree's. The witness is
//! the script below (fixture repo + fixture sibling, no cargo, no network): a
//! dirty proof.toml must not move what "GREEN at <sha>" covers, and a committed
//! pin that names an absent rev must still fail.

use std::process::Command;

#[test]
fn the_fence_reads_the_fenced_revisions_pins() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("bash")
        .arg(format!("{root}/scripts/test-fence.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-fence.sh");
    assert!(
        status.success(),
        "the pin witness failed: the fence must read proof.toml from the fenced sha"
    );
}
