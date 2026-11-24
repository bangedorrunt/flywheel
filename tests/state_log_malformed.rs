//! bd-ihiqo.6: a normal command with old log shapes in its state dir must be
//! quiet on stderr. The fake log is seeded inside TestSandbox; no daemon.

use flywheel_test_sandbox::TestSandbox;

#[test]
fn malformed_log_lines_do_not_warn_on_a_normal_command() {
    let sandbox = TestSandbox::new();
    let dir = sandbox.root().join("state/flywheel");
    std::fs::create_dir_all(&dir).expect("state dir");
    std::fs::write(
        dir.join("demo.jsonl"),
        concat!(
            r#"{"ts":"1","kind":"already-pulled","project":"demo","data":{}}"#,
            "\n",
            r#"{"ts":"2","project":"demo","data":[]}"#,
            "\n",
            r#"{"ts":"3","kind":"spawn","project":"demo","data":["old","shape"]}"#,
            "\n",
            r#"{"ts":"4","kind":"spawn","project":"demo","data":{}}"#,
            "\n"
        ),
    )
    .expect("seed legacy log");

    let out = sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .args(["runs", "list", "--json"])
        .output()
        .expect("run flywheel runs list");
    assert!(
        out.status.success(),
        "a read command must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("malformed log line"),
        "old shapes must not warn: {stderr}"
    );
    assert!(
        stderr.trim().is_empty(),
        "a normal command prints nothing on stderr: {stderr}"
    );
}
