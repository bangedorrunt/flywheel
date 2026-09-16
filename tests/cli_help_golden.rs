//! CLI help-text goldens (bd-1qqo): the binary surface is the public seam,
//! so `--help` output is pinned here — any wording/flag change fails loud.
//! Regenerate after an intentional CLI-surface change:
//! `cargo build && ./target/debug/flywheel --help > tests/fixtures/help/root.txt`
//! (same for `mail --help`, `workflow --help`).

use std::process::Command;

fn help_output(args: &[&str]) -> String {
    let exe = env!("CARGO_BIN_EXE_flywheel");
    let out = Command::new(exe)
        .args(args)
        .output()
        .expect("flywheel binary runs");
    assert!(
        out.status.success(),
        "--help must exit 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("--help is UTF-8")
}

fn assert_golden(fixture: &str, args: &[&str]) {
    let expected =
        std::fs::read_to_string(format!("tests/fixtures/help/{fixture}")).expect("golden fixture");
    let actual = help_output(args);
    assert_eq!(
        actual, expected,
        "--help output drifted; intentional changes re-capture the fixture"
    );
}

#[test]
fn help_root_matches_golden() {
    assert_golden("root.txt", &["--help"]);
}

#[test]
fn help_mail_matches_golden() {
    assert_golden("mail.txt", &["mail", "--help"]);
}

#[test]
fn help_workflow_matches_golden() {
    assert_golden("workflow.txt", &["workflow", "--help"]);
}
