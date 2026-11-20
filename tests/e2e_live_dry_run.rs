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

/// bd-w5tgg: the live five must compile from a clean export of one revision,
/// never the dirty shared tree. The fixture dirties an unrelated file and
/// checks the export carries only the commit, with the sha in the receipt.
#[test]
fn the_live_five_runs_from_a_clean_export() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("bash")
        .arg(format!("{root}/scripts/test-e2e-live-export.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-e2e-live-export.sh");
    assert!(status.success(), "the clean-export witness failed");
}
