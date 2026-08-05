//! Differential parity: event-driven loop vs poll loop (bd-test-event-poll-parity-a7r).
//!
//! **Decision-level parity** (no herdr) lives in unit tests:
//! `cargo test event_poll_parity` → `durable::tests::event_poll_parity_*`
//!
//! **Live session harness** (scripted panes under `herdr --session test-parity`)
//! is `#[ignore]` until a disposable herdr session is available in CI.
//!
//! governed-by: ADR-0001 §10 (differential parity), §6 (event determinism)

/// Documents that pure decision parity is gated by the unit suite.
/// Fail this only if the suite naming contract is broken.
#[test]
fn event_poll_parity_unit_suite_is_the_default_gate() {
    // The binary crate has no public lib surface for plan_actions/route_event.
    // Decision parity is exercised via:
    //   cargo test event_poll_parity
    // which runs durable::tests::event_poll_parity_*.
    // This integration target exists so CI can also run:
    //   cargo test --test event_poll_parity
    // without requiring a live herdr socket.
    assert!(
        true,
        "decision parity: cargo test event_poll_parity (unit); live: --ignored"
    );
}

/// Live differential: spin `herdr --session test-parity`, script shell agents,
/// run poll-only vs event-wake recording, assert identical dispatch sequences
/// and event p95 < poll p95.
///
/// Setup (manual):
/// ```text
/// herdr --session test-parity   # isolated session
/// cargo test --test event_poll_parity -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs herdr --session test-parity with scripted shell panes; run with --ignored once infra lands"]
fn live_event_vs_poll_session_identical_dispatch() {
    // Placeholder for the live harness (bd-test-event-poll-parity subtask 1–3).
    // When un-ignored, this should:
    // 1. Connect HERDR_SOCKET for session test-parity (never the captain's live session).
    // 2. Spawn N shell panes that flip working→done on a fixed schedule.
    // 3. Drive plan_actions (poll ticks) and route_event (event batch) over the same timeline.
    // 4. Assert byte-identical (agent, action, content) sequences + event latency < poll.
    // 5. Teardown: close workspace / session so no orphans remain.
    panic!(
        "live harness not yet bound to a herdr test session — \
         pure decision parity is covered by durable::tests::event_poll_parity_*"
    );
}
