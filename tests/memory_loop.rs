//! bd-97uwg: the lesson loop end to end against a TEMP chie data dir.
//! `TestSandbox` pins XDG_DATA_HOME/XDG_CONFIG_HOME/HOME under a throwaway root
//! and scrubs routing env, so the live chie store is never opened.

use flywheel_test_sandbox::TestSandbox;
use std::process::Output;

fn run(sandbox: &TestSandbox, args: &[&str]) -> Output {
    // bd-7szny: EVERY memory command in this file goes through a fake chie.
    // Without this the real installed binary runs under a temp HOME and the
    // proof depends on the operator's chiebukuro version.
    let fake = sandbox.bin_dir().join("fake-chie");
    if !fake.exists() {
        let store = sandbox.root().join("fake-chie-lessons.jsonl");
        let script = format!(
            "#!/bin/sh\nSTORE='{}'\ncase \"$2\" in\n  record) for last; do :; done\n    printf '%s\\n' \"$last\" >> \"$STORE\"\n    printf '{{\"recorded\":true}}\\n' ;;\n  recall)\n    printf '{{\"conclusions\":['\n    sep=''\n    while IFS= read -r line; do\n      
        esc=$(printf '%s' \"$line\" | sed 's/\\\\/\\\\\\\\/g; s/\"/\\\\\"/g')\n        printf '%s{{\"content\":\"%s\"}}' \"$sep\" \"$esc\"\n        sep=','\n    done < \"$STORE\" 2>/dev/null\n    printf '],\"events\":[]}}\\n' ;;\n  *) printf '[]\\n' ;;\nesac\nexit 0\n",
            store.display()
        );
        std::fs::write(&fake, script).expect("write fake chie");
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&fake).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake, perms).expect("chmod");
    }
    sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .env("FLYWHEEL_CHIE_BIN", &fake)
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

/// A fake chie: `record` succeeds, `recall` replays the two real trap shapes
/// (old and new chiebukuro renderings) from a fixture, everything else is
/// empty. The real installed chie is never spawned (bd-7szny).
fn fake_chie_with_fixture(sandbox: &TestSandbox, fixture: &str) -> std::path::PathBuf {
    let fake = sandbox.bin_dir().join("fake-chie-traps");
    let script = format!(
        "#!/bin/sh\ncase \"$2\" in\n  record) printf '{{\"recorded\":true}}\\n' ;;\n  recall) cat '{fixture}' ;;\n  *) printf '[]\\n' ;;\nesac\nexit 0\n"
    );
    std::fs::write(&fake, script).expect("write fake chie");
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = std::fs::metadata(&fake).expect("meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).expect("chmod");
    fake
}

fn fake_chie_with_traps(sandbox: &TestSandbox) -> std::path::PathBuf {
    fake_chie_with_fixture(
        sandbox,
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/chie/old-new-traps.json"
        ),
    )
}

fn run_with_chie(sandbox: &TestSandbox, fake: &std::path::Path, args: &[&str]) -> Output {
    sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .env("FLYWHEEL_CHIE_BIN", fake)
        .args(args)
        .output()
        .expect("spawn flywheel")
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
    let path_hit = run(
        &sandbox,
        &[
            "memory",
            "check-trap",
            "src/cmd/memory.rs",
            "--project",
            "proj",
            "--json",
        ],
    );
    assert!(
        text(&path_hit).contains("ambient config leaks"),
        "a path-named lesson must come back by path: {}",
        text(&path_hit)
    );
}

/// `--as` must reach chie as --peer (chie has no --pin), and the lesson must
/// still be retrievable (bd-97uwg).
fn fake_chie_logger(sandbox: &TestSandbox, log: &std::path::Path) -> std::path::PathBuf {
    let fake = sandbox.bin_dir().join("fake-chie-logger");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\ncase \"$2\" in\n  record) printf '{{\"recorded\":true}}\\n' ;;\n  recall) printf '%s\\n' '{{\"conclusions\":[{{\"content\":\"Rule: an unattributed lesson breaks when a pin cannot reach chie; do map --as to --peer\"}}],\"events\":[]}}' ;;\n  *) printf '[]\\n' ;;\nesac\nexit 0\n",
        log.display()
    );
    std::fs::write(&fake, script).expect("write fake chie logger");
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = std::fs::metadata(&fake).expect("meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).expect("chmod");
    fake
}

#[test]
fn recording_with_as_is_retrievable() {
    let sandbox = TestSandbox::new();
    let log = sandbox.bin_dir().join("chie-argv.log");
    let fake = fake_chie_logger(&sandbox, &log);
    let recorded = run_with_chie(
        &sandbox,
        &fake,
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
    let argv = std::fs::read_to_string(&log).expect("chie argv log");
    assert!(
        argv.contains("--peer agent:BoldCliff"),
        "--as must map to chie's --peer agent:<pin>: {argv}"
    );
    assert!(!argv.contains("--pin"), "chie has no --pin flag: {argv}");
    let found = run_with_chie(
        &sandbox,
        &fake,
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

/// Review point 4: repeated records leave duplicate events, but preflight must
/// show each lesson once.
#[test]
fn recalled_lessons_are_deduplicated() {
    let sandbox = TestSandbox::new();
    let fake = fake_chie_with_traps(&sandbox);
    let lesson = "a zebra marker breaks when a dedup probe runs; do keep one lesson";
    // The two real renderings from the fixture differ only in "Rule:" depth and
    // trap framing; they are one lesson and must surface once (bd-7szny).
    for content in [
        lesson,
        "Rule: a zebra marker breaks when a dedup probe runs; do keep one lesson",
        "Rule: Rule: a zebra marker breaks when a dedup probe runs; do keep one lesson",
    ] {
        let recorded = run_with_chie(
            &sandbox,
            &fake,
            &[
                "memory",
                "record-lesson",
                content,
                "--project",
                "proj",
                "--json",
            ],
        );
        assert!(
            text(&recorded).contains("\"recorded\": true"),
            "recording failed: {}",
            text(&recorded)
        );
    }
    let found = run_with_chie(
        &sandbox,
        &fake,
        &[
            "memory",
            "check-trap",
            "dedup probe",
            "--project",
            "proj",
            "--json",
        ],
    );
    assert!(
        text(&found).contains("\"count\": 1"),
        "preflight must show one copy: {}",
        text(&found)
    );
}

/// Distinct lessons that differ only inside a backticked identifier must both
/// surface; the old/new rendering of one lesson still dedupes to one (bd-7szny).
#[test]
fn distinct_code_lessons_both_surface() {
    let sandbox = TestSandbox::new();
    let fake = fake_chie_with_fixture(
        &sandbox,
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/chie/distinct-code-lessons.json"
        ),
    );
    let found = run_with_chie(
        &sandbox,
        &fake,
        &[
            "memory",
            "check-trap",
            "dedup probe",
            "--project",
            "proj",
            "--json",
        ],
    );
    let out = text(&found);
    assert!(
        out.contains("\"count\": 3"),
        "one duplicate + two distinct: {out}"
    );
    assert!(out.contains("use `a`"), "alpha lesson missing: {out}");
    assert!(out.contains("use `b`"), "beta lesson missing: {out}");
}

/// A test that shells the real chie is ambient-state dependent: proof67 broke
/// when chiebukuro changed its trap rendering (bd-7szny). Every test file that
/// actually invokes a memory command must pin FLYWHEEL_CHIE_BIN.
#[test]
fn no_test_shells_the_real_chie() {
    let root = env!("CARGO_MANIFEST_DIR");
    let mut bad = Vec::new();
    for entry in std::fs::read_dir(format!("{root}/tests")).expect("tests dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read test");
        // A helper that pins FLYWHEEL_CHIE_BIN is not enough: an inline
        // spawn in any single test still reaches the operator's chie.
        for body in src.split("#[test]").skip(1) {
            let body = body.split("\n#[test]").next().unwrap_or(body);
            let mut rest = body;
            while let Some(i) = rest.find(".command(") {
                let seg = &rest[i..(i + 1_200).min(rest.len())];
                if seg.contains("\"memory\"") && !seg.contains("FLYWHEEL_CHIE_BIN") {
                    bad.push(format!(
                        "{}:{}",
                        path.file_name().unwrap().to_string_lossy(),
                        body.lines().next().unwrap_or("<test>").trim()
                    ));
                }
                rest = &rest[i + 9..];
            }
        }
    }
    assert!(bad.is_empty(), "tests invoking real chie: {bad:?}");
}

/// A chie that exits 0 with non-JSON recall output must make check-trap fail,
/// not silently report "no traps" (bd-ndbnv review point a).
#[test]
fn malformed_recall_output_is_a_loud_failure() {
    let sandbox = TestSandbox::new();
    let fake = sandbox.bin_dir().join("fake-chie-badjson");
    std::fs::write(
        &fake,
        "#!/usr/bin/env bash\ncase \"$*\" in\n  *recall*) printf 'not json\\n' ;;\n  *) printf '[]\\n' ;;\nesac\nexit 0\n",
    )
    .expect("write fake chie");
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = std::fs::metadata(&fake).expect("meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).expect("chmod");
    let out = sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .env("FLYWHEEL_CHIE_BIN", fake.to_string_lossy().as_ref())
        .args(["memory", "check-trap", "cargo test"])
        .output()
        .expect("check-trap");
    assert!(
        !out.status.success(),
        "malformed recall JSON must not read clean: {}",
        text(&out)
    );
    assert!(
        text(&out).contains("recall") || text(&out).contains("JSON"),
        "the refusal names the bad recall: {}",
        text(&out)
    );
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
        .env("FLYWHEEL_CHIE_BIN", sandbox.bin_dir().join("fake-chie"))
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
