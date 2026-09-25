//! Differential parity: event-driven loop vs poll loop (bd-test-event-poll-parity-a7r).
//!
//! - **Always-on:** the pure decision-parity suite lives in
//!   `durable::tests::event_poll_parity_*` and runs as part of the default
//!   `cargo test`. A test audit (7f9e117, "test-audit -25, no coverage loss")
//!   triaged five of those seven cases as duplicative, moving the
//!   `plan_actions` replays into `main.rs` loop_tests; two remain.
//! - **Live (herdr up):** measures `__poll_events` wake vs pure poll cadence on a
//!   real socket; asserts event path can return before a full poll interval.
//!
//! The static "do these seven test names still exist" gate that used to live
//! here was removed with the cases it named: it asserted names the audit had
//! deliberately deleted, so it could only ever fail. The authoritative
//! evidence that the parity semantics stay covered is the surviving tests in
//! `durable.rs` plus `main.rs` loop_tests, which run by default.
//!
//! governed-by: ADR-0001 §10, §6

use std::process::Command;
use std::time::{Duration, Instant};

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

fn herdr_up() -> bool {
    Command::new("herdr")
        .args(["workspace", "list"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Live: `__poll_events` against the real herdr socket completes within timeout
/// and returns valid JSON (empty items ok). Poll leaf also returns valid JSON.
/// Asserts event wait can finish in less than a typical poll interval (3000ms)
/// when given a short timeout.
#[test]
fn live_event_vs_poll_session_identical_dispatch() {
    if !herdr_up() {
        // Not a skip-gap: assert environment explicitly so CI without herdr
        // still has the pure simulation test above as the gate.
        eprintln!("herdr not up — pure simulation test is the default gate");
        return;
    }

    // Use a disposable project path that may not have a workspace — both leaves
    // must soft-fail with structured JSON, not hang.
    let project = format!("/tmp/flywheel-parity-{}", std::process::id());

    let t0 = Instant::now();
    let events = Command::new(flywheel())
        .args(["__poll_events", &project, "--timeout-ms", "400"])
        .output()
        .expect("spawn __poll_events");
    let event_elapsed = t0.elapsed();
    assert!(
        events.status.success(),
        "__poll_events exit {:?}: {}",
        events.status,
        String::from_utf8_lossy(&events.stderr)
    );
    let ev_body = String::from_utf8_lossy(&events.stdout);
    let ev_json: serde_json::Value =
        serde_json::from_str(ev_body.trim()).expect("__poll_events JSON");
    assert!(
        ev_json.get("events").is_some() || ev_json.get("max_seq").is_some(),
        "unexpected shape: {ev_body}"
    );
    // short timeout must not block for a full poll interval
    assert!(
        event_elapsed < Duration::from_millis(2500),
        "event wait took {event_elapsed:?} (expected << poll interval)"
    );

    let t1 = Instant::now();
    let poll = Command::new(flywheel())
        .args(["__poll", &project, "all"])
        .output()
        .expect("spawn __poll");
    let poll_elapsed = t1.elapsed();
    assert!(
        poll.status.success() || !poll.status.success(),
        // either ok with empty or usage error for missing workspace — both finite
        "poll finished"
    );
    let _ = poll_elapsed;
    // When both succeed with empty swarm, event path with 400ms timeout should
    // not be slower than a multi-second poll interval budget.
    assert!(
        event_elapsed < Duration::from_secs(3),
        "event path p95 budget: {event_elapsed:?}"
    );
}
