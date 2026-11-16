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

/// The reservation board's fence (bd-0uzrq.12): the Rust fix lives in toron
/// (@51e27bde, pinned), and this model is what says a truncated read may not
/// grant. TLC runs in verify-formal, not in this gate, so this pins the two
/// halves that would otherwise drift silently: the grant guard and the
/// registration of both violations.
#[test]
fn proof_reservation_board_model_fences_the_grant() {
    let models = format!("{}/.agents/skills/verify-protocol-tla/models", root());
    let model = std::fs::read_to_string(format!("{models}/ReservationBoard.tla"))
        .expect("ReservationBoard.tla");
    assert!(
        model.contains(
            r#"[] OTHER                    -> v.status = "complete" /\ v.seen = {} /\ held = {}"#
        ),
        "a grant must need a complete read AND an empty board (the per-coordinate lock)"
    );
    let formal = std::fs::read_to_string(format!("{}/scripts/verify-formal.sh", root()))
        .expect("scripts/verify-formal.sh");
    assert!(
        formal.contains("expect_pass ReservationBoard ReservationBoard"),
        "the model must run positively in verify-formal"
    );
    for broken in ["broken-truncated-grants", "broken-no-refresh"] {
        assert!(
            formal.contains(&format!("ReservationBoard.{broken}")),
            "the {broken} fixture must be expected to violate its invariant"
        );
    }
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

/// Every quality-gate leg denies warnings (bd-0uzrq.4). A plain `cargo check`
/// compiled src/cmd/crew.rs:919's dead binding and reported success; only the
/// `-D warnings` build caught it.
#[test]
fn proof_every_gate_leg_denies_warnings() {
    let src = std::fs::read_to_string(format!("{}/scripts/verify-clippy.sh", root()))
        .expect("scripts/verify-clippy.sh");
    assert!(
        src.lines()
            .any(|l| l.contains("export RUSTFLAGS=") && l.contains("-D warnings")),
        "the gates must export RUSTFLAGS with -D warnings for every leg"
    );
    let mut legs = 0;
    for line in src.lines() {
        let line = line.trim();
        if !(line.contains("cargo check") || line.contains("cargo clippy")) {
            continue;
        }
        legs += 1;
        if line.contains("cargo clippy") {
            assert!(
                line.contains("-D warnings"),
                "clippy must deny warnings on its own line: {line}"
            );
        }
    }
    assert!(legs >= 4, "expected every gate leg, found {legs}");
}
