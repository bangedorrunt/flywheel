//! The proof harness's own two rules (bd-jp9z1.5): the suite step must run the
//! `tests/` targets for real, and `verify-mutations.sh` must never edit the tree
//! it was started from.

use std::process::Command;

fn root() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}

/// clean-proof's suite step ran only `--bin flywheel`, so 42 integration test
/// files were compiled by `no-run` and never executed: `tests/cli_help_golden`
/// was red at HEAD while a proof was green.
#[test]
fn proof_suite_step_runs_the_tests_directory() {
    let src = std::fs::read_to_string(format!("{}/scripts/clean-proof.sh", root()))
        .expect("scripts/clean-proof.sh");
    let step = src
        .lines()
        .find(|line| line.contains("run_step suite "))
        .expect("a suite step exists");
    assert!(
        step.contains("--tests"),
        "the suite step must run the tests/ targets too (bd-jp9z1.5): {step}"
    );
}

/// The harness edits source in place. Started from a shared working tree it
/// planted mutants under every other lane's build; one of them ran the real
/// launchctl. It must run in its own scratch export instead.
#[test]
fn proof_verify_mutations_never_edits_the_shared_tree() {
    let status = Command::new("bash")
        .arg(format!(
            "{}/scripts/test-verify-mutations-isolation.sh",
            root()
        ))
        .current_dir(root())
        .status()
        .expect("run scripts/test-verify-mutations-isolation.sh");
    assert!(
        status.success(),
        "scripts/test-verify-mutations-isolation.sh failed"
    );
}
