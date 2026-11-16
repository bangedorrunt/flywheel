//! bd-4z7mb follow-up: `e2e-live.sh --five --dry-run` prints its plan and
//! touches nothing. The script below drives it against a fake rex on PATH.

use std::process::Command;

#[test]
fn the_live_five_dry_run_plans_without_creating_a_session() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("bash")
        .arg(format!("{root}/scripts/test-e2e-live-dry.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-e2e-live-dry.sh");
    assert!(status.success(), "the dry-run witness failed");
}
