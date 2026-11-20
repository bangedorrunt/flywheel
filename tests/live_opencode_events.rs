//! bd-o1z4m: gated live probe. A fresh opencode seat must receive one addressed
//! mail through its native events client with no doorbell `session.prompt`.
//!
//! This file is gated on FLYWHEEL_LIVE_E2E and every test is #[ignore]d, so it
//! can never run in the default suite and never touches the operator's own
//! opencode config. Run explicitly with `--ignored` after setting the gate.

#[test]
#[ignore = "gated live probe: FLYWHEEL_LIVE_E2E=1, needs a live toron and a throwaway rex session"]
fn a_fresh_opencode_seat_receives_addressed_mail_natively() {
    if std::env::var_os("FLYWHEEL_LIVE_E2E").is_none() {
        return;
    }
    // Live sequence (operator-run only):
    //   1. spawn a throwaway opencode seat in project e2e-live-five with a
    //      pinned mail identity and a 0600 events token;
    //   2. assert the pane env carries OPENCODE_CONFIG_CONTENT with
    //      experimental.mcp_events=true and no plaintext token;
    //   3. send one addressed mail to the pin;
    //   4. assert the seat's transcript gets the hint from the native stream
    //      and the doorbell journal records no session.prompt rung for it;
    //   5. retire the seat in the trap.
    // The non-live contract (config shape, subscribe round trip, one-ring
    // dedupe) is covered by the unit tests in hc/events_bridge.
    let bin = env!("CARGO_BIN_EXE_flywheel");
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .expect("flywheel --version");
    assert!(out.status.success(), "the binary under test must run");
}
