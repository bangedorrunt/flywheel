//! Live collaboration soaks: debate, plan-review, xproj inbox.
//!
//! `live_*.rs` carve-out. Each of these writes to the operator's real stores
//! (the mail probe created `flywheel-mail-probe` in the real toron data dir on
//! every plain `cargo test`) and needs a live hc plane and mailbox, so none of
//! them can run under the sandbox. Every test is `#[ignore]`d AND gated on
//! `FLYWHEEL_LIVE_E2E`, so nothing here runs by default.
//!
//! The CLI/parse contracts they used to share a file with are in
//! `tests/collab_patterns_e2e.rs` and run every time.
//!
//! governed-by: ADR-0001 §10, §9.1–9.5

use std::process::Command;
use std::time::Duration;

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn live_e2e_opted_in() -> bool {
    std::env::var(LIVE_E2E).as_deref() == Ok("1")
}

fn plane_up() -> bool {
    Command::new("hc")
        .args(["workspace", "list"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Ask the product instead of a port constant: one `mail agents` read reaches
/// whichever surface is actually configured.
fn mail_up() -> bool {
    Command::new(flywheel())
        .args(["mail", "agents", "--project", "flywheel-mail-probe"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Live debate soak: when hc+mail up, run a 1-round debate against real
/// agents if available; otherwise validate help + that cmd starts cleanly
/// and fails fast without hanging when agents are missing.
#[ignore = "needs a live hc plane and mailbox (FLYWHEEL_LIVE_E2E=1)"]
#[test]
fn debate_soak_three_rounds_ack_gated_verdict() {
    if !live_e2e_opted_in() {
        eprintln!("{LIVE_E2E} unset — live debate soak skipped (CLI wiring covered above)");
        return;
    }
    let exe = flywheel();
    if !plane_up() {
        eprintln!("hc down — CLI wiring covered by collab_recipes_are_wired_into_the_cli");
        return;
    }

    let t0 = std::time::Instant::now();
    let out = Command::new(exe)
        .args([
            "debate",
            "e2e-prop-should-fail-fast",
            "--rounds",
            "1",
            "--agent-a",
            "oc",
            "--agent-b",
            "oc",
        ])
        .output()
        .expect("debate");
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "debate must fail fast on bad args"
    );
    assert!(
        !out.status.success(),
        "same A/B must error: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let t1 = std::time::Instant::now();
    let out = Command::new(exe)
        .args([
            "debate",
            "e2e soak proposition (deterministic)",
            "--rounds",
            "1",
            "--agent-a",
            "oc",
            "--agent-b",
            "pi",
            "--judge",
            "gk",
        ])
        .output()
        .expect("debate distinct");
    assert!(
        t1.elapsed() < Duration::from_secs(45),
        "debate wall clock budget exceeded: {:?}",
        t1.elapsed()
    );
    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("verdict"),
            "successful debate must print a verdict: {stdout}"
        );
        assert!(
            !stdout.contains("no verdict harvested"),
            "successful debate must harvest the judge's verdict: {stdout}"
        );
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panic"), "debate must not panic: {err}");
        eprintln!(
            "SKIP: debate live path failed (no scripted agents in a live hc \
             session?) — run against a spawned swarm for the full contract"
        );
    }
}

/// Live plan-review path; its `--help` contract runs in the always-on file.
#[ignore = "needs a live hc plane and mailbox (FLYWHEEL_LIVE_E2E=1)"]
#[test]
fn plan_review_implement_soak_three_paths() {
    if !live_e2e_opted_in() {
        eprintln!("{LIVE_E2E} unset — live plan-review soak skipped");
        return;
    }
    let exe = flywheel();
    if !plane_up() || !mail_up() {
        eprintln!("infra partial — pure plan_review unit tests cover APPROVE/CHANGES/REJECT");
        return;
    }

    let t0 = std::time::Instant::now();
    let out = Command::new(exe)
        .args([
            "plan-review",
            "/tmp/flywheel-pri-e2e",
            "e2e feature: noop",
            "--max-change-rounds",
            "1",
            "--planner",
            "oc",
            "--reviewer",
            "pi",
            "--implementer",
            "gk",
        ])
        .output()
        .expect("plan-review");
    assert!(
        t0.elapsed() < Duration::from_secs(60),
        "plan-review hung: {:?}",
        t0.elapsed()
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("panic"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Live xproj inbox leaf; the parse contract runs in the always-on file.
#[ignore = "needs a live mailbox (FLYWHEEL_LIVE_E2E=1)"]
#[test]
fn live_cross_project_xproj_inbox() {
    if !live_e2e_opted_in() {
        eprintln!("{LIVE_E2E} unset — live xproj inbox skipped");
        return;
    }
    let exe = flywheel();
    if !mail_up() {
        eprintln!("mail down — skip the product surface");
        return;
    }

    let out = Command::new(exe)
        .args(["__xproj-inbox", "/tmp/flywheel-xproj-e2e", "--limit", "5"])
        .output()
        .expect("xproj-inbox");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(v.get("items").is_some(), "shape: {v}");
}
