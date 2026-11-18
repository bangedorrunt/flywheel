//! bd-97uwg: the lesson loop end to end against a TEMP chie data dir.
//! `TestSandbox` pins XDG_DATA_HOME/XDG_CONFIG_HOME/HOME under a throwaway root
//! and scrubs routing env, so the live chie store is never opened.

use flywheel_test_sandbox::TestSandbox;
use std::process::Output;

fn run(sandbox: &TestSandbox, args: &[&str]) -> Output {
    sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .args(args)
        .output()
        .expect("spawn flywheel")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A plain `record-lesson` (no --kind) writes a Rule/Trap/Proof lesson chie's
/// guard can match, and the same slug reads it back.
#[test]
fn a_plain_lesson_is_retrievable_by_command_and_path() {
    let sandbox = TestSandbox::new();
    let by_cmd = "a poisoned ambient config breaks when `cargo test` runs without a temp HOME; do redirect all three roots";
    let recorded = run(
        &sandbox,
        &[
            "memory",
            "record-lesson",
            by_cmd,
            "--project",
            "proj",
            "--json",
        ],
    );
    assert!(
        recorded.status.success() && text(&recorded).contains("\"recorded\": true"),
        "recording failed: {}",
        text(&recorded)
    );

    let found = run(
        &sandbox,
        &[
            "memory",
            "check-trap",
            "cargo test",
            "--project",
            "proj",
            "--json",
        ],
    );
    assert!(found.status.success(), "check failed: {}", text(&found));
    assert!(
        text(&found).contains("poisoned ambient config"),
        "a plain lesson must come back through check-trap: {}",
        text(&found)
    );
    assert!(
        found.stdout.starts_with(b"{"),
        "the check must emit JSON: {}",
        text(&found)
    );

    let by_path = "`src/cmd/memory.rs` breaks when ambient config leaks into a spawned binary; do pin temp roots";
    assert!(
        run(
            &sandbox,
            &["memory", "record-lesson", by_path, "--project", "proj"],
        )
        .status
        .success()
    );
    let path_hit = sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .args([
            "memory",
            "check-trap",
            "src/cmd/memory.rs",
            "--project",
            "proj",
            "--json",
        ])
        .output()
        .expect("path check");
    assert!(
        text(&path_hit).contains("ambient config leaks"),
        "a path-named lesson must come back by path: {}",
        text(&path_hit)
    );
}

/// `--as` must reach chie as --peer (chie has no --pin), and the lesson must
/// still be retrievable (bd-97uwg).
#[test]
fn recording_with_as_is_retrievable() {
    let sandbox = TestSandbox::new();
    let recorded = run(
        &sandbox,
        &[
            "memory",
            "record-lesson",
            "an unattributed lesson breaks when a pin cannot reach chie; do map --as to --peer",
            "--as",
            "BoldCliff",
            "--project",
            "proj",
            "--json",
        ],
    );
    assert!(
        text(&recorded).contains("\"recorded\": true"),
        "recording with --as failed: {}",
        text(&recorded)
    );
    let found = run(
        &sandbox,
        &[
            "memory",
            "check-trap",
            "a pin cannot reach chie",
            "--project",
            "proj",
            "--json",
        ],
    );
    assert!(
        text(&found).contains("unattributed lesson"),
        "the --as lesson must be retrievable: {}",
        text(&found)
    );
}

/// bd-ndbnv: lessons recorded the migration way (decision kind, slug Trap
/// line) must surface by natural preflight query — across DIFFERENT shapes,
/// not one fitted phrase.
#[test]
fn migrated_lessons_are_check_trap_findable_by_natural_query() {
    let sandbox = TestSandbox::new();
    let cases = [
        (
            "Rule: A backoff is not a rate limit without single-flight — the incident prose.\nTrap: `a-backoff-is-not-a-rate-limit` — A backoff is not a rate limit without single-flight\nProof: migrated at b06a4ff8^",
            "backoff single-flight",
            "incident prose",
        ),
        (
            "Rule: A park is interruption, and the SDK already had the vocabulary — DispatchAcquire Park can hand back.\nTrap: `bd-ynef-park-is-interruption` — A park is interruption\nProof: migrated at b06a4ff8^",
            "park is interruption",
            "sdk already had the vocabulary",
        ),
        (
            "Rule: Grilling Jido to toron-workflow port plan, an audit and interview — contracts beat runtime.\nTrap: `jido-port-grilling` — Jido port grilling\nProof: migrated at b06a4ff8^",
            "jido port grilling",
            "contracts beat runtime",
        ),
        (
            "Rule: Never deploy without a proof — the close needs a legal ledger verdict.\nTrap: `never-deploy-without-proof` — Never deploy without a proof\nProof: migrated at b06a4ff8^",
            "never deploy without a proof",
            "legal ledger verdict",
        ),
    ];
    for (lesson, _, needle) in cases {
        let recorded = run(
            &sandbox,
            &[
                "memory",
                "record-lesson",
                lesson,
                "--kind",
                "decision",
                "--project",
                "proj",
                "--json",
            ],
        );
        assert!(
            text(&recorded).contains("\"recorded\": true"),
            "recording failed for {needle}: {}",
            text(&recorded)
        );
    }
    for (_, query, needle) in cases {
        let found = run(
            &sandbox,
            &["memory", "check-trap", query, "--project", "proj", "--json"],
        );
        assert!(
            text(&found).to_ascii_lowercase().contains(needle),
            "migrated lesson ({needle}) must surface by natural query: {}",
            text(&found)
        );
    }
}

/// `--project <slug>` and `--project <dir>` name the same chie scope.
#[test]
fn slug_and_directory_scopes_agree() {
    let sandbox = TestSandbox::new();
    let project_dir = sandbox.root().join("proj");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    let dir = project_dir.to_string_lossy();
    let lesson = "hermetic cli roots break when a spawned binary inherits ambient config; do redirect all three roots";
    let recorded = run(
        &sandbox,
        &[
            "memory",
            "record-lesson",
            lesson,
            "--project",
            &dir,
            "--json",
        ],
    );
    assert!(
        text(&recorded).contains("\"recorded\": true"),
        "recording by directory failed: {}",
        text(&recorded)
    );

    let sandbox_root = sandbox.root().to_string_lossy();
    let by_slug = sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .env("FLYWHEEL_ROOT", sandbox_root.as_ref())
        .args([
            "memory",
            "check-trap",
            "a spawned binary inherits ambient config",
            "--project",
            "proj",
        ])
        .output()
        .expect("slug check");
    assert!(
        text(&by_slug).contains("hermetic cli roots"),
        "a slug must find a lesson recorded by directory: {}",
        text(&by_slug)
    );
}

/// A chie usage error is loud; an unreachable daemon is still clean.
#[test]
fn usage_errors_are_loud_but_unreachable_is_clean() {
    let sandbox = TestSandbox::new();
    let bin = sandbox.bin_dir();
    let bad = bin.join("fake-chie-usage");
    std::fs::write(
        &bad,
        "#!/usr/bin/env bash\necho 'error: unexpected argument' >&2\nexit 2\n",
    )
    .expect("write fake chie");
    let down = bin.join("fake-chie-down");
    std::fs::write(
        &down,
        "#!/usr/bin/env bash\necho 'connect refused' >&2\nexit 1\n",
    )
    .expect("write fake chie");
    for p in [&bad, &down] {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(p).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(p, perms).expect("chmod");
    }

    let usage = sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .env("FLYWHEEL_CHIE_BIN", bad.to_string_lossy().as_ref())
        .args(["memory", "check-trap", "cargo test"])
        .output()
        .expect("usage check");
    assert!(!usage.status.success(), "a usage error must not read clean");
    assert!(
        text(&usage).contains("unexpected argument"),
        "the refusal names the chie error: {}",
        text(&usage)
    );

    let unreachable = sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .env("FLYWHEEL_CHIE_BIN", down.to_string_lossy().as_ref())
        .args(["memory", "check-trap", "cargo test"])
        .output()
        .expect("down check");
    assert!(
        unreachable.status.success()
            && String::from_utf8_lossy(&unreachable.stdout).trim() == "no traps",
        "an unreachable daemon is still clean: {}",
        text(&unreachable)
    );
}
