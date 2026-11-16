//! captain watch preflight arm (bd-captain-watch-preflight-arm-snak) — the
//! call-site proof for `cmd::captain::first_failing_check`.
//!
//! The unit test pins the pure selection rule; this drives the REAL binary so
//! the wiring is proven too (a helper-only test stays green when the arm is
//! deleted from the watch loop). The run is sandboxed (bd-jp9z1.6): an empty
//! HOME/XDG and a PATH of deny-shims only, so the arm's deterministic failure
//! is the pane plane (no rex, no hc socket) rather than whatever the operator's
//! machine happens to have. It runs BEFORE the mail setup, so no inbox is read.

//! governed-by: bd-captain-watch-preflight-arm-snak

use flywheel_test_sandbox::TestSandbox;

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// The arm exits 4 (its own code, distinct from 1 usage and 3 stall) and emits
/// one JSON object naming the failing check and its message — the remedy line.
#[test]
fn captain_watch_exits_on_preflight_failure_e2e() {
    let sandbox = TestSandbox::new();
    let out = sandbox
        .command(flywheel())
        .args([
            "captain",
            "watch",
            "/nonexistent-flywheel-preflight-arm-proj",
            "--preflight",
            "--preflight-mode",
            "steer",
            "--json",
        ])
        .output()
        .expect("spawn flywheel");

    let code = out.status.code();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        code,
        Some(4),
        "the preflight arm exits 4 (not 1/3): stdout={stdout} stderr={stderr}"
    );
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("json on stdout ({e}): {stdout}"));
    assert_eq!(
        v["reason"], "preflight",
        "the exit reason is the preflight arm"
    );
    let check = v["check"].as_str().expect("a failing check is named");
    assert!(!check.is_empty(), "the failing check is named: {v}");
    let msg = v["message"]
        .as_str()
        .expect("the remedy message rides along");
    assert!(
        !msg.is_empty(),
        "the failing check's message is rendered: {v}"
    );
    // A hermetic sandbox has no rex and no hc socket, so the first check that
    // can fail is the pane plane; the arm names whichever check it reached.
    assert!(
        matches!(check, "project-dir" | "hc-socket" | "pane-plane"),
        "the named failure is one the real preflight produces: {v}"
    );
}
