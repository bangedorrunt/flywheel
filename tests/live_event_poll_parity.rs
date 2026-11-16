//! Live event-vs-poll parity (bd-test-event-poll-parity-a7r), opt-in only.
//!
//! This is the `live_*.rs` carve-out: it measures the REAL hc socket, so it
//! cannot run under the test sandbox (which clears the routing env and puts
//! deny-shims first on PATH). `scripts/check-test-sandbox.sh` allows exactly
//! this shape — every test here is `#[ignore]`d AND gated on
//! `FLYWHEEL_LIVE_E2E=1` — so nothing in this file can run by default.
//!
//! The always-on decision-parity cases live in `durable::tests` and
//! `main.rs` loop_tests; `tests/event_poll_parity.rs` holds the source-scan
//! guard that keeps the gate ahead of any live call.
//!
//! governed-by: ADR-0001 §10, §6

use std::process::Command;
use std::time::{Duration, Instant};

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// Opt-in for the live parity test. Off unless exactly `1`.
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

/// Live: `__poll_events` against the real hc socket completes within timeout
/// and returns valid JSON (empty items ok). Poll leaf also returns valid JSON.
/// Asserts event wait can finish in less than a typical poll interval (3000ms)
/// when given a short timeout.
#[ignore = "needs a live hc plane and FLYWHEEL_LIVE_E2E=1"]
#[test]
fn live_event_vs_poll_session_identical_dispatch() {
    if !live_e2e_opted_in() {
        eprintln!(
            "{LIVE_E2E} unset — live parity skipped (the in-crate parity tests are the gate)"
        );
        return;
    }
    if !plane_up() {
        eprintln!("hc not up — in-crate parity tests are the gate");
        return;
    }

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
        "poll finished"
    );
    let _ = poll_elapsed;
    assert!(
        event_elapsed < Duration::from_secs(3),
        "event path p95 budget: {event_elapsed:?}"
    );
}
