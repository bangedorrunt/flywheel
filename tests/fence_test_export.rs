//! bd-8jakq: the fence's targeted-test leg must run inside the clean export,
//! so another lane's uncommitted WIP in the shared tree cannot break it. The
//! shell witness uses a fake cargo and a throwaway repo — no real build.

use flywheel_test_sandbox::TestSandbox;

#[test]
fn fence_test_export_witness() {
    let root = env!("CARGO_MANIFEST_DIR");
    let sandbox = TestSandbox::new();
    let real_home = std::env::var("HOME").unwrap_or_default();
    let status = sandbox
        .command("/bin/bash")
        .env("PATH", sandbox.path_env())
        .env("RUSTUP_HOME", format!("{real_home}/.rustup"))
        .env("CARGO_HOME", format!("{real_home}/.cargo"))
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
    let sandbox = TestSandbox::new();
    let real_home = std::env::var("HOME").unwrap_or_default();
    let stock = sandbox
        .command("/bin/bash")
        .env("PATH", sandbox.path_env())
        .env("RUSTUP_HOME", format!("{real_home}/.rustup"))
        .env("CARGO_HOME", format!("{real_home}/.cargo"))
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
        !fence.contains("mapfile -t"),
        "fence.sh must stay bash 3.2 safe — no mapfile (bd-250g4)"
    );
}
