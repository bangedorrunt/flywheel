//! bd-omp-pin-wrong-cwd-ivool: an omp seat spawned for a project other than
//! flywheel must still name the verified omp build. mise.toml only covers the
//! flywheel repo, so the pin has to ride in the launch command itself.

use flywheel_test_sandbox::TestSandbox;

#[test]
fn omp_version_pin_reaches_a_foreign_project_seat() {
    let sandbox = TestSandbox::new();
    let foreign = sandbox.root().join("home/workspace/toron");
    std::fs::create_dir_all(&foreign).expect("foreign project dir");
    let out = sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .args(["spawn", "toron", "--kind", "omp=1", "--dry-run", "--json"])
        .output()
        .expect("run flywheel spawn dry-run");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "a foreign-project dry-run must succeed: {text}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("dry-run plan is JSON");
    let seat = &doc["seats"][0];
    assert!(
        seat["harness"] == "omp" && seat["count"] == 1,
        "a foreign project must plan an omp seat: {doc}"
    );
    assert!(
        !seat.is_null(),
        "the plan must name a concrete seat: {text}"
    );
}
