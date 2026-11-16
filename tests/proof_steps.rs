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

/// bd-proof-toron-bin: the suite step builds the pinned sibling toron and the
/// tests that need it pass; the mutation stage runs in verify-mutations' own
/// scratch export, whose ../toron link points at the WORKSPACE tree. Proven at
/// f5971320: adr0027-s1-receipts passed the suite and its mutation control
/// panicked with "toron CLI not found at <export>/flywheel/../toron/...".
/// clean-proof must hand the built path to the mutation stage, and the
/// isolation re-exec must keep it absolute.
#[test]
fn proof_mutation_stage_hands_the_built_toron_to_the_mutations() {
    let clean = std::fs::read_to_string(format!("{}/scripts/clean-proof.sh", root()))
        .expect("scripts/clean-proof.sh");
    // The handed-over path must be a file the PROOF owns. In the export,
    // toron/target is a symlink into mbx's shared cache, which mbx evicts
    // during a multi-hour proof: proof38 handed over a path that dangled
    // ("run failed: NotFound" at tests/adr0027_slice1.rs:121).
    assert!(
        clean.contains(r#"cp "$TMP/toron/target/debug/toron" "$TMP/bin/toron""#),
        "clean-proof must copy the built CLI out of the evictable target dir"
    );
    assert!(
        clean.contains(r#"TORON_BIN_BUILT="$TMP/bin/toron""#),
        "and hand over the copy, not a path inside the shared cache"
    );
    assert!(
        !clean.contains(r#"TORON_BIN_BUILT="$TMP/toron/target/debug/toron""#),
        "the target path may dangle by the time the mutation stage runs"
    );
    assert!(
        clean.contains(r#"[ -n "$TORON_BIN_BUILT" ] && proof_env+=("TORON_BIN=$TORON_BIN_BUILT")"#),
        "the mutation stage must receive TORON_BIN"
    );
    assert!(
        clean.contains(r#"env "${proof_env[@]}" bash scripts/verify-mutations.sh"#),
        "and that environment must be the one the harness runs under"
    );
    let harness = std::fs::read_to_string(format!("{}/scripts/verify-mutations.sh", root()))
        .expect("scripts/verify-mutations.sh");
    assert!(
        harness.contains(r#"TORON_BIN="$toron_bin" VERIFY_MUTATIONS_IN_EXPORT=1 exec"#),
        "the isolation re-exec must keep the caller's absolute TORON_BIN"
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

/// The mutation harness must call an aborted run what it is (bd-0uzrq.24 /
/// proof33): a breach that panicked during unwind aborted the process, and
/// NO-RESULTS sent the reader hunting a CARGO_ARGS bug that did not exist.
#[test]
fn proof_mutation_harness_classifies_an_abort() {
    let out = Command::new("bash")
        .arg(format!("{}/scripts/verify-mutations.sh", root()))
        .arg("--self-test")
        .current_dir(root())
        .output()
        .expect("run the mutation harness self-test");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "self-test failed: {text}");
    assert!(
        text.contains("ABORTED      a run that aborts after starting"),
        "the harness must classify an abort as ABORTED: {text}"
    );
    assert!(
        text.contains("the abort names the test that was running"),
        "the ABORTED verdict must name the running test: {text}"
    );
}
