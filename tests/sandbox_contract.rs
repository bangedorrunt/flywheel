//! bd-jp9z1.4: the shared test sandbox's own contract, plus the lint that
//! refuses a test spawning the built binary without it.

use flywheel_test_sandbox::{SCRUBBED, TestSandbox};
use std::process::Command;

/// A sandbox whose deny-shim was reached must fail on Drop — the live
/// launchctl/systemctl must never run from a test.
#[test]
#[should_panic(expected = "reached the real supervisor")]
fn drop_panics_when_a_supervisor_shim_was_reached() {
    let sandbox = TestSandbox::new();
    let shim = sandbox.bin_dir().join("launchctl");
    let _ = Command::new(shim).arg("bootstrap").status();
    drop(sandbox);
}

/// Opting in is deliberate, and then Drop stays quiet.
#[test]
fn allow_supervisor_calls_keeps_drop_quiet() {
    let sandbox = TestSandbox::new();
    sandbox.allow_supervisor_calls();
    let _ = Command::new(sandbox.bin_dir().join("systemctl")).status();
    assert!(!sandbox.shim_log().is_empty(), "the shim did run");
    drop(sandbox);
}

/// A child command inherits nothing: the live routing vars are not in its
/// environment and the sandbox vars are.
#[test]
fn command_does_not_inherit_the_live_routing_env() {
    let sandbox = TestSandbox::new();
    let cmd = sandbox.command("/usr/bin/env");
    let keys: Vec<String> = cmd
        .get_envs()
        .filter_map(|(k, _)| k.to_str().map(str::to_string))
        .collect();
    for key in SCRUBBED {
        assert!(!keys.iter().any(|k| k == key), "{key} must not be set");
    }
    for key in ["HOME", "XDG_STATE_HOME", "TORON_DATA_DIR", "PATH"] {
        assert!(keys.iter().any(|k| k == key), "{key} must be set");
    }
}

/// The lint refuses an offender and passes a file that uses the helper.
#[test]
fn lint_refuses_a_test_that_spawns_the_binary_without_the_helper() {
    let root = env!("CARGO_MANIFEST_DIR");
    let self_test = Command::new("bash")
        .arg(format!("{root}/scripts/check-test-sandbox.sh"))
        .arg("--self-test")
        .status()
        .expect("run lint self-test");
    assert!(self_test.success(), "the lint's own self-test must pass");
    let scan = Command::new("bash")
        .arg(format!("{root}/scripts/check-test-sandbox.sh"))
        .current_dir(root)
        .status()
        .expect("run lint");
    assert!(scan.success(), "the repo must have no unsandboxed spawn");
}
