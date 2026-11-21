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
    // bd-250g4: some crews resolve `bash` to macOS /bin/bash 3.2, which has
    // no mapfile. The same witness must pass there too.
    let stock = Command::new("/bin/bash")
        .arg(format!("{root}/scripts/test-fence.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-fence.sh under /bin/bash");
    assert!(
        stock.success(),
        "the fence witness must also pass under /bin/bash 3.2 (no mapfile)"
    );
    let fence =
        std::fs::read_to_string(format!("{root}/scripts/fence.sh")).expect("scripts/fence.sh");
    assert!(
        !fence.contains("mapfile"),
        "fence.sh must stay bash 3.2 safe — no mapfile (bd-250g4)"
    );
}
