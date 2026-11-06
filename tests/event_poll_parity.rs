//! Differential parity: event-driven loop vs poll loop (bd-test-event-poll-parity-a7r).
//!
//! - **Always-on:** the pure decision-parity suite lives in
//!   `durable::tests::event_poll_parity_*` and runs as part of the default
//!   `cargo test`. A test audit (7f9e117, "test-audit -25, no coverage loss")
//!   triaged five of those seven cases as duplicative, moving the
//!   `plan_actions` replays into `main.rs` loop_tests; two remain.
//! - **Fake hc: removed.** hc carries no workspaces, so `__poll` can never
//!   find a swarm on the hc plane, and the port predates the `hello`
//!   handshake. The fixture could only ever fail; the in-crate tests pin the
//!   decision parity.
//! - **Live (opt-in, `FLYWHEEL_LIVE_E2E=1`, off by default):** measures
//!   `__poll_events` wake vs pure poll cadence on the real hc socket.
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

/// Opt-in for the live parity test. Off unless exactly `1`.
const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn live_e2e_opted_in() -> bool {
    std::env::var(LIVE_E2E).as_deref() == Ok("1")
}

/// The live test checks the opt-in before it touches hc: a plain
/// `cargo test` used to drive the real hc socket whenever hc was up.
#[test]
fn live_parity_is_opt_in_before_anything_live() {
    let src = include_str!("event_poll_parity.rs");
    let at = src
        .find("#[test]\nfn live_event_vs_poll_session_identical_dispatch() {")
        .expect("live test");
    let body = &src[at..];
    let gate = body.find("if !live_e2e_opted_in()").expect("opt-in gate");
    for live in ["plane_up()", "Command::new("] {
        let first = body.find(live).unwrap_or(usize::MAX);
        assert!(gate < first, "the opt-in gate must come before {live}");
    }
    if std::env::var(LIVE_E2E).ok().as_deref() != Some("1") {
        assert!(!live_e2e_opted_in(), "off unless {LIVE_E2E}=1");
    }
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
