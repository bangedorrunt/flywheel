//! LIVE bd-64qoy witness: one addressed mail to a provisioned antigravity seat
//! rides `agy agentapi send-message` with NO pane keystrokes, and a forced
//! agentapi failure falls back to Inject — with BOTH routes journaled before
//! and after the invoke.
//!
//! Live carve-out: every test here is `#[ignore]`d and the file is gated on
//! `FLYWHEEL_LIVE_E2E`, so it never runs by default.
//!
//! ```text
//! FLYWHEEL_LIVE_E2E=1 \
//! FLYWHEEL_LIVE_PROJECT=flywheel \
//! FLYWHEEL_LIVE_AGY_PIN=<the provisioned no-door seat> \
//! FLYWHEEL_LIVE_AGY_CONVERSATION=<conversation id from spawn> \
//! FLYWHEEL_LIVE_STATE_DIR=<the flywheel state dir> \
//!   cargo test -p flywheel --test live_agy_agentapi -- --ignored --nocapture
//! ```

use std::process::Command;

fn gate() -> Option<(String, String, std::path::PathBuf)> {
    if std::env::var("FLYWHEEL_LIVE_E2E").ok().as_deref() != Some("1") {
        return None;
    }
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some((
        var("FLYWHEEL_LIVE_PROJECT")?,
        var("FLYWHEEL_LIVE_AGY_PIN")?,
        std::path::PathBuf::from(var("FLYWHEEL_LIVE_STATE_DIR")?),
    ))
}

fn send(project: &str, pin: &str, marker: &str, agentapi_exe: Option<&str>) {
    let mut cmd = Command::new("flywheel");
    cmd.args(["send", project, marker, "--to", pin]);
    if let Some(exe) = agentapi_exe {
        cmd.env("ANTIGRAVITY_AGENTAPI_EXE", exe);
    }
    let out = cmd.output().expect("flywheel send");
    assert!(
        out.status.success(),
        "flywheel send failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn journal_len(state_dir: &std::path::Path) -> usize {
    std::fs::read_to_string(state_dir.join("wake-routes.jsonl"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// The seed is the watermark (`wm-…`), not the marker text, so read only rows
/// written after the send.
fn route_rows(state_dir: &std::path::Path, pin: &str, from: usize) -> Vec<String> {
    let raw = std::fs::read_to_string(state_dir.join("wake-routes.jsonl")).unwrap_or_default();
    raw.lines()
        .skip(from)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["pin"] == pin)
        .filter_map(|v| Some(format!("{}:{}", v["phase"].as_str()?, v["route"].as_str()?)))
        .collect()
}

#[test]
#[ignore]
fn an_addressed_mail_rides_agentapi_with_no_pane_keystrokes() {
    let Some((project, pin, state_dir)) = gate() else {
        eprintln!(
            "bd-64qoy live witness skipped: set FLYWHEEL_LIVE_E2E=1, FLYWHEEL_LIVE_PROJECT, FLYWHEEL_LIVE_AGY_PIN, FLYWHEEL_LIVE_STATE_DIR"
        );
        return;
    };
    let marker = format!("agy-api-{}", std::process::id());
    let from = journal_len(&state_dir);
    send(&project, &pin, &marker, None);
    let rows = route_rows(&state_dir, &pin, from);
    assert!(
        rows.contains(&"intent:api".to_string()),
        "the api route must be journaled before invoke: {rows:?}"
    );
    assert!(
        rows.contains(&"outcome:api".to_string()),
        "one mail via agentapi, no Inject fallback: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("inject")),
        "an API wake must never type into the pane: {rows:?}"
    );
}

#[test]
#[ignore]
fn a_failed_agentapi_falls_back_to_inject_and_journals_both() {
    let Some((project, pin, state_dir)) = gate() else {
        return;
    };
    let marker = format!("agy-fallback-{}", std::process::id());
    let from = journal_len(&state_dir);
    // A guaranteed-failing transport: the doorbell must fall back to Inject.
    send(&project, &pin, &marker, Some("/usr/bin/false"));
    let rows = route_rows(&state_dir, &pin, from);
    assert!(
        rows.contains(&"intent:api".to_string()),
        "the api intent must still be journaled: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.starts_with("outcome:inject")),
        "a failed agentapi must fall back to Inject: {rows:?}"
    );
}
