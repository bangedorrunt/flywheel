//! LIVE bd-b2kaa witness on a real codex seat: fire the installed Stop hook
//! four times with mail waiting and assert exactly three continuation lines,
//! then verify the hook-reported turn state is what the doorbell reads.
//!
//! Live carve-out: `#[ignore]`d and gated on `FLYWHEEL_LIVE_E2E`.
//!
//! ```text
//! FLYWHEEL_LIVE_E2E=1 FLYWHEEL_LIVE_PROJECT=flywheel \
//! FLYWHEEL_LIVE_CODEX_PIN=<the provisioned codex seat> \
//! FLYWHEEL_LIVE_STATE_DIR=<the flywheel state dir> \
//!   cargo test -p flywheel --test live_codex_hooks -- --ignored --nocapture
//! ```

use std::process::Command;

fn gate() -> Option<(String, String, std::path::PathBuf)> {
    if std::env::var("FLYWHEEL_LIVE_E2E").ok().as_deref() != Some("1") {
        return None;
    }
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some((
        var("FLYWHEEL_LIVE_PROJECT")?,
        var("FLYWHEEL_LIVE_CODEX_PIN")?,
        std::path::PathBuf::from(var("FLYWHEEL_LIVE_STATE_DIR")?),
    ))
}

fn hook(project: &str, pin: &str, event: &str) -> String {
    let out = Command::new("flywheel")
        .args(["hook", event, "--project", project, "--pin", pin])
        .output()
        .expect("flywheel hook");
    assert!(
        out.status.success(),
        "flywheel hook {event} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
#[ignore]
fn a_codex_stop_hook_continues_at_most_three_times() {
    let Some((project, pin, state_dir)) = gate() else {
        eprintln!(
            "bd-b2kaa live witness skipped: set FLYWHEEL_LIVE_E2E=1, FLYWHEEL_LIVE_PROJECT, FLYWHEEL_LIVE_CODEX_PIN, FLYWHEEL_LIVE_STATE_DIR"
        );
        return;
    };
    // The doorbell writes this signal when mail is queued for the seat.
    let signal = state_dir
        .join("turn-hooks")
        .join(format!("{}-{}", project, pin))
        .join("mail.json");
    std::fs::create_dir_all(signal.parent().unwrap()).unwrap();
    std::fs::write(&signal, r#"{"waiting":true,"urgent":false}"#).unwrap();

    let lines: Vec<String> = (0..4).map(|_| hook(&project, &pin, "stop")).collect();
    assert_eq!(
        lines.iter().filter(|l| !l.is_empty()).count(),
        3,
        "Stop continues at most three times: {lines:?}"
    );

    hook(&project, &pin, "session-start");
    let turn = state_dir
        .join("turn-hooks")
        .join(format!("{}-{}", project, pin))
        .join("turn.json");
    let raw = std::fs::read_to_string(&turn).unwrap_or_default();
    assert!(
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v["running"].as_bool())
            .unwrap_or(false),
        "SessionStart must report a running turn: {raw}"
    );
}
