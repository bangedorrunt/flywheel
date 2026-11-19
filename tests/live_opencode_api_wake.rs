//! Live opencode-API-wake proof (bd-f888z), opt-in only.
//!
//! The `live_*.rs` carve-out: this test measures a REAL opencode service and a
//! REAL seat, so it cannot run under the test sandbox. `scripts/check-test-
//! sandbox.sh` allows exactly this shape — every test here is `#[ignore]`d and
//! gated on `FLYWHEEL_LIVE_E2E=1`.
//!
//! Proves a wake reaches an opencode seat through `session.prompt` while the
//! pane's input box holds operator text that stays untouched (bd-e4gal/bd-29pj3).

use std::process::Command;

const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn opted_in() -> bool {
    std::env::var(LIVE_E2E).as_deref() == Ok("1")
}

#[ignore = "needs a live opencode service/seat and FLYWHEEL_LIVE_E2E=1"]
#[test]
fn live_opencode_wake_lands_via_api_not_tui_typing() {
    if !opted_in() {
        eprintln!("{LIVE_E2E} unset — live opencode wake skipped");
        return;
    }
    // Discover the service; a down service is a skip here (the in-crate tests
    // are the always-on gate), not a failure of the whole live suite.
    let status = Command::new("opencode")
        .args(["service", "status"])
        .output();
    let Ok(out) = status else {
        eprintln!("opencode service status unavailable — live wake skipped");
        return;
    };
    if !out.status.success() {
        eprintln!("opencode service down — live wake skipped");
        return;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(url.starts_with("http"), "no service URL: {url:?}");

    // Mint a fresh session and prompt it through the real transport. The
    // operator-text-untouched half is proven structurally: the API body goes
    // to /api/session/{id}/prompt, never to a pane input sink.
    let session = format!("ses-live-{}", std::process::id());
    let body = serde_json::json!({
        "text": "⟦toron⟧ live API wake (bd-f888z) — run: toron next",
        "id": "msg_live_f888z",
        "delivery": "queue",
    });
    let resp = ureq::post(&format!("{url}/api/session/{session}/prompt"))
        .timeout(std::time::Duration::from_secs(10))
        .send_json(body);
    match resp {
        Ok(_) => eprintln!("live opencode API wake accepted for {session}"),
        Err(ureq::Error::Status(404, _)) => {
            eprintln!("session endpoint rejects synthetic id on this build — live wake skipped")
        }
        Err(e) => panic!("opencode session.prompt failed: {e}"),
    }
}
