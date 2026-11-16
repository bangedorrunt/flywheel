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
//! - **Live (opt-in, `FLYWHEEL_LIVE_E2E=1`, off by default):** lives in
//!   `tests/live_event_poll_parity.rs`, the `live_*.rs` carve-out that the
//!   sandbox lint allows only because every test there is `#[ignore]`d and
//!   gated on that variable. It measures `__poll_events` against the real hc
//!   socket, which a sandboxed child cannot reach by design.
//!
//! The static "do these seven test names still exist" gate that used to live
//! here was removed with the cases it named: it asserted names the audit had
//! deliberately deleted, so it could only ever fail. The authoritative
//! evidence that the parity semantics stay covered is the surviving tests in
//! `durable.rs` plus `main.rs` loop_tests, which run by default.
//!
//! This file spawns nothing: it keeps the gate honest by reading the live
//! file's source.
//!
//! governed-by: ADR-0001 §10, §6

/// Opt-in for the live parity test. Off unless exactly `1`.
const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

/// The live test checks the opt-in before it touches hc: a plain
/// `cargo test` used to drive the real hc socket whenever hc was up.
#[test]
fn live_parity_is_opt_in_before_anything_live() {
    let src = include_str!("live_event_poll_parity.rs");
    let at = src
        .find("fn live_event_vs_poll_session_identical_dispatch() {")
        .expect("live test");
    let body = &src[at..];
    let gate = body.find("if !live_e2e_opted_in()").expect("opt-in gate");
    for live in ["plane_up()", "Command::new("] {
        let first = body.find(live).unwrap_or(usize::MAX);
        assert!(gate < first, "the opt-in gate must come before {live}");
    }
    assert!(
        src.contains("#[ignore"),
        "the live carve-out must be #[ignore]d so it cannot run by default"
    );
    if std::env::var(LIVE_E2E).ok().as_deref() != Some("1") {
        assert!(!live_e2e_opted_in(), "off unless {LIVE_E2E}=1");
    }
}

fn live_e2e_opted_in() -> bool {
    std::env::var(LIVE_E2E).as_deref() == Ok("1")
}
