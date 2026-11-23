//! bd-u6o2l: toron's commit fence must refuse a staged warning and pass a
//! clean commit. The shell witness runs in a throwaway clone; no live toron
//! repo, daemon or operator state is touched.

use std::process::Command;

#[test]
fn toron_commit_fence_refuses_a_warning() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("bash")
        .arg(format!("{root}/../toron/scripts/test-fence.sh"))
        .current_dir(root)
        .env_remove("AGENT_NAME")
        .env_remove("FLYWHEEL_COMMIT_TOKEN")
        .env_remove("FLYWHEEL_COMMIT_OPERATOR")
        .status()
        .expect("run toron scripts/test-fence.sh");
    assert!(
        status.success(),
        "toron's staged-index fence must refuse the warning and prove the clean commit"
    );
}
