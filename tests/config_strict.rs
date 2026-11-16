//! flywheel config parse-or-refuse (wave12-d): hermetic CLI-level tests.
//!
//! Spawns the built binary with a cleared env + temp HOME/XDG/CWD so no
//! operator config, `.env`, or state leaks in. For each migrated knob a
//! garbage value exits nonzero naming the accepted forms; a valid value
//! behaves as before (exit 0, no refusal).

use std::process::Output;

use flywheel_test_sandbox::TestSandbox;

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

fn hermetic() -> TestSandbox {
    let sandbox = TestSandbox::new();
    std::fs::create_dir_all(sandbox.root().join("work")).expect("work dir");
    sandbox
}

fn run_with_env(sandbox: &TestSandbox, envs: &[(&str, &str)]) -> Output {
    let mut cmd = sandbox.command(flywheel());
    // The binary shells out for some config paths; the shims still come first.
    cmd.env("PATH", sandbox.path_env());
    cmd.current_dir(sandbox.root().join("work"));
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(["config", "list"]).output().expect("run flywheel")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn config_strict_assert_refuses(knob: &str, garbage: &str, accepted: &str) {
    let h = hermetic();
    let out = run_with_env(&h, &[(knob, garbage)]);
    assert!(
        !out.status.success(),
        "{knob}={garbage:?} must fail, stderr: {}",
        stderr(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains(knob) && err.contains(accepted),
        "{knob}={garbage:?} must name the knob and accepted forms ({accepted}), stderr: {err}"
    );
}

fn config_strict_assert_accepts(knob: &str, valid: &str) {
    let h = hermetic();
    let out = run_with_env(&h, &[(knob, valid)]);
    assert!(
        out.status.success(),
        "{knob}={valid:?} must succeed, stderr: {}",
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("is not valid"),
        "{knob}={valid:?} must not refuse, stderr: {}",
        stderr(&out)
    );
}

#[test]
fn config_strict_startup_knobs_refuse_garbage() {
    let table: &[(&str, &str, &str, &str)] = &[
        (
            "FLYWHEEL_BACKEND",
            "bogus",
            "auto|rex|superlogical|hc",
            "hc",
        ),
        ("FLYWHEEL_MAIL", "pigeon", "toron", "toron"),
        ("FLYWHEEL_CREW_MAX", "many", "an integer >= 1", "5"),
        ("FLYWHEEL_CREW_MAX", "0", "an integer >= 1", "5"),
        ("FLYWHEEL_TIMER", "perhaps", "1|true|on|0|false|off", "0"),
        (
            "FLYWHEEL_ALLOW_EXEC",
            "perhaps",
            "1|true|on|0|false|off",
            "1",
        ),
        (
            "FLYWHEEL_NO_DOORBELL_ENSURE",
            "perhaps",
            "1|true|on|0|false|off",
            "1",
        ),
        ("FLYWHEEL_SHIP_GATE_MODE", "sometimes", "block|warn", "warn"),
        (
            "FLYWHEEL_SUPERVISOR_JOIN",
            "crowd",
            "all|quorum:N|first:N",
            "quorum:2",
        ),
        (
            "FLYWHEEL_SUPERVISOR_MAX_WALL_MS",
            "long",
            "non-negative integer",
            "60000",
        ),
        (
            "FLYWHEEL_SUPERVISOR_STRATEGY",
            "chaos",
            "one_for_one|rest_for_one|one_for_all",
            "one_for_one",
        ),
        (
            "FLYWHEEL_SUPERVISOR_TICK_MS",
            "soon",
            "non-negative integer",
            "1000",
        ),
        ("FLYWHEEL_INBOX_TICK_MS", "soon", "an integer > 0", "1000"),
    ];
    for (knob, garbage, accepted, valid) in table {
        config_strict_assert_refuses(knob, garbage, accepted);
        config_strict_assert_accepts(knob, valid);
    }
}

#[test]
fn config_strict_valid_config_behaves_as_before() {
    // No knob set: hermetic defaults load and the command succeeds.
    let h = hermetic();
    let out = run_with_env(&h, &[]);
    assert!(
        out.status.success(),
        "clean env must succeed, stderr: {}",
        stderr(&out)
    );
}
