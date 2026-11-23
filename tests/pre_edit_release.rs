//! bd-lk8s2: behaviour proof of pre-edit lease release. A fake `toron` inside
//! TestSandbox records every argv; the real CLI drives `commit` and `crew done`
//! so the assertion is about a release CALL, not a line of source text.

use std::path::Path;
use std::process::{Command, Output};

use flywheel_test_sandbox::TestSandbox;

const PIN: &str = "PurpleRidge";

fn fake_toron_with(sandbox: &TestSandbox, fail_release: bool) {
    let path = sandbox.bin_dir().join("toron");
    let tail = if fail_release {
        "[ \"$1 $2\" = 'reserve release' ] && exit 1"
    } else {
        "true"
    };
    std::fs::write(
        &path,
        format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"$TORON_ARGV_LOG\"\n\
             case \"$1 $2\" in\n  'reserve conflicts') printf '%s\\n' '{{\"conflicts\":[],\"soft_conflicts\":[]}}' ;;\n\
             esac\n{tail}\nexit 0\n"
        ),
    )
    .expect("fake toron");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
}

fn flywheel(sandbox: &TestSandbox, args: &[&str], cwd: Option<&Path>) -> Output {
    let mut cmd = sandbox.command(env!("CARGO_BIN_EXE_flywheel"));
    cmd.env("PATH", sandbox.path_env())
        .env("TORON_ARGV_LOG", sandbox.root().join("toron-argv.log"))
        .env("AGENT_NAME", PIN)
        .env("FLYWHEEL_PANE", "1")
        .args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.output().expect("run flywheel")
}

fn argv_log(sandbox: &TestSandbox) -> String {
    std::fs::read_to_string(sandbox.root().join("toron-argv.log")).unwrap_or_default()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn seed_seat(sandbox: &TestSandbox, project: &str) {
    let dir = sandbox.root().join("state/flywheel/agents");
    std::fs::create_dir_all(&dir).expect("agents dir");
    let row = serde_json::json!([{
        "pane_id": "block:test",
        "name": "test-seat",
        "harness": "opencode",
        "mail_identity": PIN,
        "skills_injected": false,
        "incarnation": 0
    }]);
    std::fs::write(dir.join(format!("{project}.json")), row.to_string()).expect("seat row");
}

#[test]
fn commit_releases_only_the_committed_paths() {
    let sandbox = TestSandbox::new();
    fake_toron_with(&sandbox, false);
    seed_seat(&sandbox, "demo");
    let repo = sandbox.root().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("seed.txt"), "seed\n").expect("seed");
    git(&repo, &["add", "seed.txt"]);
    git(&repo, &["commit", "-qm", "seed"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").expect("a");
    std::fs::write(repo.join("b.rs"), "fn b() {}\n").expect("b");
    git(&repo, &["add", "b.rs"]);

    let out = flywheel(
        &sandbox,
        &[
            "commit",
            "a.rs",
            "-m",
            "bd-lk8s2 witness",
            "--repo",
            repo.to_str().unwrap(),
        ],
        Some(&repo),
    );
    assert!(
        out.status.success(),
        "commit must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = argv_log(&sandbox);
    assert!(
        log.lines()
            .any(|l| l == format!("reserve release --project demo --as {PIN} --paths a.rs --json")),
        "the committed path must be released: {log}"
    );
    let pre_edit_releases: Vec<&str> = log
        .lines()
        .filter(|l| l.starts_with("reserve release --project demo"))
        .collect();
    assert_eq!(
        pre_edit_releases,
        vec![format!("reserve release --project demo --as {PIN} --paths a.rs --json").as_str()],
        "exactly the committed path is released, not the staged foreign one: {log}"
    );
}

#[test]
fn crew_done_releases_the_brief_pins_fallback_leases() {
    let sandbox = TestSandbox::new();
    fake_toron_with(&sandbox, false);
    let briefs = sandbox.root().join("state/flywheel/briefs");
    std::fs::create_dir_all(&briefs).expect("briefs dir");
    let book = serde_json::json!({"briefs": [{
        "id": "brief-lk8s2", "pin": PIN, "subject": "witness", "opened_ts_us": 1_u64
    }]});
    std::fs::write(briefs.join("demo.json"), book.to_string()).expect("brief book");

    let out = flywheel(
        &sandbox,
        &[
            "crew",
            "done",
            "brief-lk8s2",
            "--receipt",
            "ok",
            "--project",
            "demo",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "crew done must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = argv_log(&sandbox);
    assert!(
        log.lines().any(|l| l
            == format!(
                "reserve release-by-reason --project demo --as {PIN} --no-notify pre-edit:{PIN}"
            )),
        "the brief's pin must release its fallback leases: {log}"
    );
}

#[test]
fn a_failing_release_logs_but_does_not_fail_the_commit() {
    let sandbox = TestSandbox::new();
    fake_toron_with(&sandbox, true);
    seed_seat(&sandbox, "demo");
    let repo = sandbox.root().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("seed.txt"), "seed\n").expect("seed");
    git(&repo, &["add", "seed.txt"]);
    git(&repo, &["commit", "-qm", "seed"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").expect("a");
    let out = flywheel(
        &sandbox,
        &[
            "commit",
            "a.rs",
            "-m",
            "bd-lk8s2 failure witness",
            "--repo",
            repo.to_str().unwrap(),
        ],
        Some(&repo),
    );
    assert!(
        out.status.success(),
        "a release failure must not fail the commit: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("pre-edit lease release after commit failed"),
        "the failure must be logged: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
