//! bd-kszuj: the proof proves the commit, not the shared tree. The witness is
//! the script below (fixture repo, no cargo, no network).

use flywheel_test_sandbox::TestSandbox;

#[test]
fn the_export_carries_the_commit_not_the_shared_tree() {
    let root = env!("CARGO_MANIFEST_DIR");
    let sandbox = TestSandbox::new();
    let status = sandbox
        .command("/bin/bash")
        .env("PATH", sandbox.path_env())
        .arg(format!("{root}/scripts/test-clean-proof-export.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-clean-proof-export.sh");
    assert!(
        status.success(),
        "the export witness failed: the proof must carry the revision, not the tree"
    );
}
