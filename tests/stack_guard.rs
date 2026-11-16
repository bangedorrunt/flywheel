//! scripts/stack.sh install is a fleet mutation: it replaces the installed
//! binaries. Until bd-0uzrq.24 it carried no guard at all, so a crew pin (or a
//! crew pane with AGENT_NAME unset) could run it straight from a worktree.
//! The sandbox script proves refusal, the operator escape, and fail-closed.

use std::process::Command;

#[test]
fn stack_script_refuses_a_crew_pin() {
    let root = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("bash")
        .arg(format!("{root}/scripts/test-stack-guard.sh"))
        .arg(env!("CARGO_BIN_EXE_flywheel"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-stack-guard.sh");
    assert!(status.success(), "scripts/test-stack-guard.sh failed");
}
