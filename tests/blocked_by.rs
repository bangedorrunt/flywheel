//! bd-6xu6r: a failed build mails the holder of the blamed file once, and
//! never mails the caller about its own file. The fake toron owns both the
//! reservation board and the mail log; no live daemon is touched.

use std::os::unix::fs::PermissionsExt;

use flywheel_test_sandbox::TestSandbox;

fn fake_toron(sandbox: &TestSandbox, holder: &str) {
    let path = sandbox.bin_dir().join("toron");
    let body = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TORON_ARGV_LOG"
if [ "$1 $2" = 'reserve list' ]; then
  printf '%s\n' '{{"reservations":[{{"path":"src/one.rs","holder":"{holder}"}}]}}'
fi
exit 0
"#
    );
    std::fs::write(&path, body).expect("fake toron");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn run(sandbox: &TestSandbox, log: &std::path::Path) -> std::process::Output {
    let mut cmd = sandbox.command(env!("CARGO_BIN_EXE_flywheel"));
    cmd.env("PATH", sandbox.path_env())
        .env("TORON_ARGV_LOG", sandbox.root().join("toron-argv.log"))
        .args([
            "blocked-by",
            "--log",
            log.to_str().unwrap(),
            "--caller",
            "PurpleRidge",
            "--project",
            "demo",
            "--json",
        ]);
    cmd.output().expect("run blocked-by")
}

fn build_log(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("build.log");
    std::fs::write(
        &path,
        "   Compiling demo v0.1.0\nerror[E0308]: mismatched types\n  --> src/one.rs:12:5\n   |\n12 | let x: u8 = \"no\";\n",
    )
    .expect("build log");
    path
}

fn argv_log(sandbox: &TestSandbox) -> String {
    std::fs::read_to_string(sandbox.root().join("toron-argv.log")).unwrap_or_default()
}

#[test]
fn a_build_break_mails_its_holder_once() {
    let sandbox = TestSandbox::new();
    fake_toron(&sandbox, "TealCove");
    let log = build_log(sandbox.root());

    let first = run(&sandbox, &log);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&first.stdout).expect("json");
    assert_eq!(v["alerted"], true);
    let log_text = argv_log(&sandbox);
    let mails: Vec<&str> = log_text
        .lines()
        .filter(|l| l.starts_with("mail send"))
        .collect();
    assert_eq!(mails.len(), 1, "one alert per break: {log_text}");
    assert!(mails[0].contains("--to TealCove"), "{}", mails[0]);
    assert!(mails[0].contains("src/one.rs"), "{}", mails[0]);
    assert!(
        mails[0].contains("E0308"),
        "the snippet must be in the mail: {}",
        mails[0]
    );

    let second = run(&sandbox, &log);
    assert!(second.status.success());
    let v: serde_json::Value = serde_json::from_slice(&second.stdout).expect("json");
    assert_eq!(v["alerted"], false, "a repeat break must be deduped");
    let after = argv_log(&sandbox);
    let mails_after: Vec<&str> = after
        .lines()
        .filter(|l| l.starts_with("mail send"))
        .collect();
    assert_eq!(
        mails_after.len(),
        1,
        "no second mail: {}",
        argv_log(&sandbox)
    );
}

#[test]
fn an_error_in_the_callers_own_file_mails_no_one() {
    let sandbox = TestSandbox::new();
    fake_toron(&sandbox, "PurpleRidge");
    let log = build_log(sandbox.root());

    let out = run(&sandbox, &log);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["alerted"], false);
    assert!(
        !argv_log(&sandbox)
            .lines()
            .any(|l| l.starts_with("mail send")),
        "never alert the caller about its own file: {}",
        argv_log(&sandbox)
    );
}

#[test]
fn a_log_without_a_compiler_error_is_rejected() {
    let sandbox = TestSandbox::new();
    fake_toron(&sandbox, "TealCove");
    let path = sandbox.root().join("clean.log");
    std::fs::write(&path, "   Compiling demo v0.1.0\n    Finished dev\n").expect("log");
    let out = run(&sandbox, &path);
    assert!(
        !out.status.success(),
        "a log with no error must fail closed"
    );
}
