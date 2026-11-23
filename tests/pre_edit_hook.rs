//! bd-lk0kt: hermetic proof of the pre-edit hook. A fake toron on PATH owns
//! the lease board; TestSandbox keeps HOME, XDG dirs and the plane isolated.
//! The hook is the real CLI (`flywheel __hook pre-edit`) driven with a Claude
//! PreToolUse payload on stdin.

use std::process::Stdio;

use flywheel_test_sandbox::TestSandbox;

fn fake_toron(sandbox: &TestSandbox) -> std::path::PathBuf {
    let bin = sandbox.bin_dir();
    let path = bin.join("toron");
    std::fs::write(
        &path,
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TORON_ARGV_LOG"
if [ "$1" = reserve ] && [ "$2" = acquire ]; then
  if [ -n "$FAKE_HOLDER" ]; then
    printf '{"conflicts":[{"path":"x","holders":["%s"]}]}\n' "$FAKE_HOLDER"
  else
    printf '{"granted":[{"path":"x"}]}\n'
  fi
  exit 0
fi
echo "unexpected toron call: $*" >&2
exit 2
"#,
    )
    .expect("fake toron");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    path
}

fn seed_seat(sandbox: &TestSandbox, project: &str, pin: &str) {
    let dir = sandbox.root().join("state/flywheel/agents");
    std::fs::create_dir_all(&dir).expect("agents dir");
    let row = serde_json::json!([{
        "pane_id": "block:test",
        "name": "test-seat",
        "harness": "claude",
        "mail_identity": pin,
        "skills_injected": false,
        "incarnation": 0
    }]);
    std::fs::write(dir.join(format!("{project}.json")), row.to_string()).expect("seat row");
}

fn pre_edit(
    sandbox: &TestSandbox,
    toron: &std::path::Path,
    pin: &str,
    payload: &str,
    holder: Option<&str>,
) -> std::process::Output {
    let argv_log = sandbox.root().join("toron-argv.log");
    let mut cmd = sandbox.command(env!("CARGO_BIN_EXE_flywheel"));
    cmd.env("PATH", sandbox.path_env())
        .env("TORON_BIN", toron)
        .env("TORON_ARGV_LOG", &argv_log)
        .env("AGENT_NAME", pin)
        .env("FLYWHEEL_PANE", "1")
        .args(["__hook", "pre-edit", "--agent", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(holder) = holder {
        cmd.env("FAKE_HOLDER", holder);
    }
    let mut child = cmd.spawn().expect("spawn hook");
    use std::io::Write as _;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    child.wait_with_output().expect("hook output")
}

fn argv_log(sandbox: &TestSandbox) -> String {
    std::fs::read_to_string(sandbox.root().join("toron-argv.log")).unwrap_or_default()
}

fn payload(path: &str) -> String {
    serde_json::json!({"tool_name":"Edit","tool_input":{"file_path":path}}).to_string()
}

#[test]
fn a_free_path_gets_the_lease() {
    let sandbox = TestSandbox::new();
    let toron = fake_toron(&sandbox);
    seed_seat(&sandbox, "joinproj", "Alpha");
    let out = pre_edit(&sandbox, &toron, "Alpha", &payload("src/main.rs"), None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let argv = argv_log(&sandbox);
    assert!(
        argv.contains(
            "reserve acquire --project joinproj --as Alpha --paths src/main.rs --exclusive --queue"
        ),
        "acquire argv: {argv}"
    );
}

#[test]
fn a_held_path_names_the_holder_and_queues_one_waiter() {
    let sandbox = TestSandbox::new();
    let toron = fake_toron(&sandbox);
    seed_seat(&sandbox, "joinproj", "Beta");
    let out = pre_edit(
        &sandbox,
        &toron,
        "Beta",
        &payload("src/main.rs"),
        Some("Alpha"),
    );
    assert_eq!(out.status.code(), Some(2), "holder must deny the edit");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("src/main.rs is leased by Alpha") && err.contains("queued"),
        "blocked message: {err}"
    );
    let argv = argv_log(&sandbox);
    assert_eq!(
        argv.lines()
            .filter(|l| l.contains("reserve acquire"))
            .count(),
        1,
        "exactly one queued acquire: {argv}"
    );
    assert!(argv.contains("--queue"), "the acquire must queue: {argv}");
}

#[test]
fn a_repeat_pre_edit_by_the_holder_adds_no_second_lease() {
    let sandbox = TestSandbox::new();
    let toron = fake_toron(&sandbox);
    seed_seat(&sandbox, "joinproj", "Alpha");
    let p = payload("src/main.rs");
    let first = pre_edit(&sandbox, &toron, "Alpha", &p, None);
    let second = pre_edit(&sandbox, &toron, "Alpha", &p, None);
    assert!(first.status.success() && second.status.success());
    // toron owns idempotence: a repeat acquire resolves to the same single
    // lease, so the hook must not mint a second event or change the outcome.
    let argv = argv_log(&sandbox);
    assert_eq!(
        argv.lines()
            .filter(|l| l.contains("reserve acquire"))
            .count(),
        2,
        "the hook asks each time: {argv}"
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).is_empty(),
        "a held path must stay silent on repeat: {}",
        String::from_utf8_lossy(&second.stderr)
    );
}

#[test]
fn a_sibling_path_is_leased_with_the_repo_prefix() {
    let sandbox = TestSandbox::new();
    let toron = fake_toron(&sandbox);
    seed_seat(&sandbox, "joinproj", "Alpha");
    let out = pre_edit(
        &sandbox,
        &toron,
        "Alpha",
        &payload("../toron/src/lib.rs"),
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        argv_log(&sandbox).contains("--paths ../toron/src/lib.rs"),
        "sibling path must keep its repo prefix: {}",
        argv_log(&sandbox)
    );
}

#[test]
fn integration_status_names_harnesses_with_a_pre_edit_hook() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/integration.rs"),
    )
    .unwrap();
    assert!(
        src.contains("pub const CLAUDE_EVENTS") && src.contains("\"PreToolUse\""),
        "claude must advertise the pre-edit hook event"
    );
    assert!(
        src.contains("pub const CODEX_EVENTS"),
        "codex must advertise its hook events too"
    );
}
