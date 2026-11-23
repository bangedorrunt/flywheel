//! The proof harness's own two rules (bd-jp9z1.5): the suite step must run the
//! `tests/` targets for real, and `verify-mutations.sh` must never edit the tree
//! it was started from.

use flywheel_test_sandbox::TestSandbox;
use std::process::Command;

fn root() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}

/// bd-b3j7x: every script witness runs under TestSandbox. A fake `toron`
/// first on PATH records its argv, so clean-proof's red-mail leg can never
/// reach the live daemon and the witness can assert where the mail went.
fn sandboxed_bash(root: &str, script: &str) -> Command {
    let sandbox = Box::leak(Box::new(TestSandbox::new()));
    let fake = sandbox.bin_dir().join("toron");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "${TORON_FAKE_LOG:?}"
exit 0
"#,
    )
    .expect("fake toron");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("fake toron mode");
    }
    let mut cmd = sandbox.command("bash");
    cmd.arg(format!("{root}/scripts/{script}"))
        .current_dir(root)
        .env("PATH", sandbox.path_env())
        // Witnesses read the shipped source through FLYWHEEL_SOURCE; the
        // sandbox's stub src/ exists only to keep an errant install harmless.
        .env("FLYWHEEL_SOURCE", root)
        .env("TORON_FAKE_LOG", sandbox.root().join("toron-calls.log"));
    cmd
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
        .find(|line| line.contains("run_parallel_step suite "))
        .expect("a suite step exists");
    assert!(
        step.contains("--tests"),
        "the suite step must run the tests/ targets too (bd-jp9z1.5): {step}"
    );
}

/// bd-9dxjt: the proof must build into a REAL target dir it owns. mbx's default
/// `target` is a symlink into its shared managed cache, which any lane's `mbx
/// gc` can evict mid-proof; the suite's self-spawning tests (`current_exe`)
/// then fail with ENOENT because their binary was unlinked under a running
/// process. A proof-owned CARGO_TARGET_DIR is not a managed target, so no sweep
/// can reach it. This asserts the structural guard, not the code under test.
#[test]
fn proof_builds_into_a_target_dir_it_owns() {
    let clean = std::fs::read_to_string(format!("{}/scripts/clean-proof.sh", root()))
        .expect("scripts/clean-proof.sh");
    assert!(
        clean.contains(r#"export CARGO_TARGET_DIR="$TMP/target""#),
        "the proof must export a real, proof-owned CARGO_TARGET_DIR (bd-9dxjt); \
         mbx's managed target is a symlink a concurrent sweep can evict"
    );
    assert!(
        clean.contains(r#"[ -L "$CARGO_TARGET_DIR" ]"#),
        "the proof must fail loudly if mbx turned the target back into a swept symlink"
    );
}

/// bd-proof-toron-bin: the suite step builds the pinned sibling toron and the
/// tests that need it pass; the mutation stage runs in verify-mutations' own
/// scratch export, whose ../toron is now (bd-5j8ri) the PINNED sibling source.
/// Proven at
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
        formal.contains("for tla in \"$MODELS\"/*.tla"),
        "the model must be discovered positively in verify-formal"
    );
    assert!(
        formal.contains("for cfg in \"$MODELS\"/*.broken-*.cfg"),
        "broken cfgs must be discovered in verify-formal"
    );
    for (broken, invariant) in [
        ("broken-truncated-grants", "NoGrantWithoutCompleteBoard"),
        ("broken-no-refresh", "AtMostOneHolder"),
    ] {
        let cfg = std::fs::read_to_string(format!("{models}/ReservationBoard.{broken}.cfg"))
            .unwrap_or_else(|_| panic!("the {broken} fixture must exist"));
        assert_eq!(
            cfg.lines().next(),
            Some(format!("\\* expect: {invariant}").as_str()),
            "the {broken} fixture must name {invariant} on its first line"
        );
    }
}

/// The harness edits source in place. Started from a shared working tree it
/// planted mutants under every other lane's build; one of them ran the real
/// launchctl. It must run in its own scratch export instead.
#[test]
fn proof_verify_mutations_never_edits_the_shared_tree() {
    let status = sandboxed_bash(root(), "test-verify-mutations-isolation.sh")
        .status()
        .expect("run scripts/test-verify-mutations-isolation.sh");
    assert!(
        status.success(),
        "scripts/test-verify-mutations-isolation.sh failed"
    );
}

/// bd-5j8ri: a verify-mutations scratch export must carry the siblings pinned
/// by the revision under test, so another lane's dirty toron worktree cannot
/// turn a flywheel mutant green or unbuildable.
#[test]
fn proof_verify_mutations_exports_pinned_siblings() {
    let status = sandboxed_bash(root(), "test-verify-mutations-pinned-siblings.sh")
        .status()
        .expect("run scripts/test-verify-mutations-pinned-siblings.sh");
    assert!(
        status.success(),
        "scripts/test-verify-mutations-pinned-siblings.sh failed"
    );
}

/// bd-feu67: a mutation is proof only when the NAMED test failed, not when
/// its name appears in a passing line beside another test's failure.
#[test]
fn proof_verify_mutations_credits_only_the_named_test() {
    let status = sandboxed_bash(root(), "test-verify-mutations-expected-test.sh")
        .status()
        .expect("run expected-test witness");
    assert!(status.success(), "expected-test verdict witness failed");
}

/// bd-08t3o: the pinned-siblings witness must ignore a build dir inherited
/// from the outer proof, or clean-proof reuses the outer export and the
/// witness becomes a no-op that fails for the wrong reason.
#[test]
fn proof_verify_mutations_ignores_an_inherited_build_dir() {
    let status = sandboxed_bash(root(), "test-verify-mutations-pinned-siblings.sh")
        .env("VERIFY_MUTATIONS_BUILD_DIR", "/tmp/fw-proof-stale/mut-tree")
        .env("VERIFY_MUTATIONS_TARGET_DIR", "/tmp/fw-proof-stale/target")
        .env("VERIFY_MUTATIONS_EXPORT_ROOT", "/tmp/fw-proof-stale/export")
        .env("VERIFY_MUTATIONS_IN_EXPORT", "1")
        .status()
        .expect("run pinned-siblings with a proof env");
    assert!(
        status.success(),
        "the witness inherited the proof build dir"
    );
}

/// bd-3681w: a kept proof export is for the captain's install. Letting every
/// kept `fw-proof.*` directory accumulate filled the disk and killed proof44.
/// bd-j45zo: exclude the kept export by basename, not by a path string whose
/// spelling changes when TMPDIR ends in '/'.
#[test]
fn proof_clean_proof_keep_prunes_older_exports() {
    let src = std::fs::read_to_string(format!("{}/scripts/clean-proof.sh", root()))
        .expect("scripts/clean-proof.sh");
    assert!(
        src.contains(r#"! -name "$(basename "$TMP")""#),
        "clean-proof must exclude the kept export by basename (bd-j45zo)"
    );
    assert!(
        src.contains(r#"if [ "$status" -eq 0 ]"#),
        "pruning must run only after a green proof (bd-j45zo)"
    );
}

/// bd-j45zo: a trailing-slash TMPDIR made `! -path "$TMP"` delete the export
/// proof46 had just kept, leaving nothing to install. The shell fixture drives
/// both that case and the red-proof case with the real cleanup() in play.
#[test]
fn proof_clean_proof_keep_survives_trailing_slash_tmpdir() {
    let status = sandboxed_bash(root(), "test-clean-proof-keep.sh")
        .status()
        .expect("run scripts/test-clean-proof-keep.sh");
    assert!(status.success(), "scripts/test-clean-proof-keep.sh failed");
}

/// bd-6setu.18: a TEST_FILTER that matches no #[test] runs zero tests, so its
/// spec reads VACUOUS instead of RED and the mutants are silently lost. The
/// needle gate must refuse it, and the fixture drives both halves.
#[test]
fn proof_verify_needles_refuses_a_dead_test_filter() {
    let status = sandboxed_bash(root(), "test-verify-needles-filters.sh")
        .status()
        .expect("run scripts/test-verify-needles-filters.sh");
    assert!(
        status.success(),
        "scripts/test-verify-needles-filters.sh failed"
    );
}

/// bd-m97cj: a mutation whose expect= is outside TEST_FILTER reads VACUOUS.
#[test]
fn proof_verify_needles_refuses_an_unselected_expect() {
    let out = sandboxed_bash(root(), "test-verify-needles-expect-filter.sh")
        .output()
        .expect("run scripts/test-verify-needles-expect-filter.sh");
    assert!(
        out.status.success(),
        "scripts/test-verify-needles-expect-filter.sh failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
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
    let out = sandboxed_bash(root(), "verify-mutations.sh")
        .arg("--self-test")
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

/// bd-xwn96: fail-fast, the receipt's failing-spec field and the shared crew
/// witness cache are all cheap to prove without a full proof run.
#[test]
fn proof_mutation_failfast_and_crew_cache() {
    let status = sandboxed_bash(root(), "test-proof-mutation-failfast.sh")
        .status()
        .expect("run scripts/test-proof-mutation-failfast.sh");
    assert!(status.success(), "proof mutation failfast test failed");
}

/// bd-2gxup: a stale committed Cargo.lock fails the proof at the build step.
#[test]
fn a_stale_lockfile_fails_the_proof_build_step() {
    let status = sandboxed_bash(root(), "test-proof-lock-stale.sh")
        .status()
        .expect("run scripts/test-proof-lock-stale.sh");
    assert!(status.success(), "stale Cargo.lock witness failed");
}

/// bd-9os20: the Claude plugin must load from its installed path and stack.sh
/// must copy it there; both are checkable without a live Claude seat.
/// bd-p7oim: the proof receipt must carry per-step wall time, so a slow suite
/// can be found from the receipt instead of reading each step's log.
#[test]
fn proof_receipt_records_each_step_seconds() {
    let status = sandboxed_bash(root(), "test-proof-step-secs.sh")
        .status()
        .expect("run scripts/test-proof-step-secs.sh");
    assert!(status.success(), "proof receipt timing witness failed");
}

/// bd-p7oim: a green sibling suite at a pinned rev must be cached by rev, so a
/// later proof at the same rev skips the slow suite but still records it.
#[test]
fn proof_sibling_suite_caches_by_rev() {
    let status = sandboxed_bash(root(), "test-proof-sibling-cache.sh")
        .status()
        .expect("run scripts/test-proof-sibling-cache.sh");
    assert!(status.success(), "sibling suite cache witness failed");
}

/// bd-p7oim: independent suites must overlap once the build is done.
#[test]
fn proof_suites_run_concurrently() {
    let status = sandboxed_bash(root(), "test-proof-parallel.sh")
        .status()
        .expect("run scripts/test-proof-parallel.sh");
    assert!(status.success(), "parallel suite witness failed");
}

#[test]
fn proof_claude_plugin_loads_and_installs_to_plugin_dir() {
    let status = sandboxed_bash(root(), "test-claude-plugin-load.sh")
        .status()
        .expect("run scripts/test-claude-plugin-load.sh");
    assert!(status.success(), "claude plugin load test failed");
}
