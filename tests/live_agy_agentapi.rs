//! LIVE bd-64qoy witness: `agy agentapi send-message` accepts a wake for a
//! provisioned antigravity conversation with no pane keystrokes. The route
//! choice, journal-before-invoke, idempotent id and Inject fallback are proved
//! by `harness_api` / `harness_caps` unit tests with a fake transport; this leg
//! only proves the real CLI door is reachable.
//!
//! Live carve-out: every test here is `#[ignore]`d and the file is gated on
//! `FLYWHEEL_LIVE_E2E`, so it never runs by default.
//!
//! ```text
//! FLYWHEEL_LIVE_E2E=1 FLYWHEEL_LIVE_AGY_CONVERSATION=<conversation-id> \
//!   cargo test -p flywheel --test live_agy_agentapi -- --ignored --nocapture
//! ```

use std::process::Command;

fn gate() -> Option<String> {
    if std::env::var("FLYWHEEL_LIVE_E2E").ok().as_deref() != Some("1") {
        return None;
    }
    std::env::var("FLYWHEEL_LIVE_AGY_CONVERSATION")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

#[test]
#[ignore]
fn an_addressed_mail_reaches_the_agentapi_door() {
    let Some(conversation) = gate() else {
        eprintln!(
            "bd-64qoy live witness skipped: set FLYWHEEL_LIVE_E2E=1 and FLYWHEEL_LIVE_AGY_CONVERSATION"
        );
        return;
    };
    let marker = format!("agy-live-{}", std::process::id());
    let bin = std::env::var("ANTIGRAVITY_AGENTAPI_EXE").unwrap_or_else(|_| "agy".to_string());
    let out = Command::new(bin)
        .args(["agentapi", "send-message", &conversation, &marker])
        .output()
        .expect("agy agentapi send-message");
    assert!(
        out.status.success(),
        "live agentapi send failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!("sent {marker} to conversation {conversation} via agentapi");
}
