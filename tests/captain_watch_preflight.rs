//! captain watch preflight arm (bd-captain-watch-preflight-arm-snak) — the
//! call-site proof for `cmd::captain::first_failing_check`.
//!
//! The unit test pins the pure selection rule; this drives the REAL binary so
//! the wiring is proven too (a helper-only test stays green when the arm is
//! deleted from the watch loop). A nonexistent project makes `project-dir`
//! fail deterministically — no herdr/resonate/toron store is required, and the
//! arm runs BEFORE the mail setup so no inbox is reached.
//!
//! governed-by: bd-captain-watch-preflight-arm-snak

use std::process::Command;

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// The arm exits 4 (its own code, distinct from 1 usage and 3 stall) and emits
/// one JSON object naming the failing check and its message — the remedy line.
#[test]
fn captain_watch_exits_on_preflight_failure_e2e() {
    // Temp dirs pinned as XDG homes so the probe cannot read the operator's
    // real config/state; the dir lives to the end of the test.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let out = Command::new(flywheel())
        .args([
            "captain",
            "watch",
            // A project that cannot exist → `project-dir` must FAIL.
            "/nonexistent-flywheel-preflight-arm-proj",
            "--preflight",
            "--preflight-mode",
            "steer",
            "--json",
        ])
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
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
    // The named failure must be one the REAL preflight can produce, so a
    // fabricated exit cannot pass this: the nonexistent project fails
    // `project-dir`, or (when herdr is unreachable) that check is named first.
    assert!(
        check == "project-dir" || check == "herdr-socket",
        "the named failure is one the real preflight produces: {v}"
    );
}
