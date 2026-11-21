//! bd-8jakq: the fence's targeted-test leg must run inside the clean export,
//! so another lane's uncommitted WIP in the shared tree cannot break it. The
//! shell witness uses a fake cargo and a throwaway repo — no real build.

use std::process::Command;

#[test]
fn fence_test_export_witness() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("bash")
        .arg(format!("{root}/scripts/test-fence.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-fence.sh");
    assert!(
        status.success(),
        "the fence export witness failed: --test must run in the export, not the tree"
    );
}
