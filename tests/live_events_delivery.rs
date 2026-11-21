//! bd-znczi: gated live proof of ADR-0039 "Events primary" (P1).
//!
//! One throwaway rex session spawns a pi, an opencode and a Claude seat. Each
//! gets one addressed mail and one urgent mail. A seat reached through MCP
//! Events leaves a `doorbell/events-<pin>.jsonl` journal row and shows the
//! marker in its pane; the doorbell records NO ring for that pin. If the per-pin
//! token mint is disabled, the events pump never starts and the doorbell rings
//! instead — the test fails naming the doorbell fallback.
//!
//! Live carve-out (AGENTS.md): every test is `#[ignore]`d AND this file is
//! gated on FLYWHEEL_LIVE_E2E, so `cargo test` never runs it by default.
//!
//!   FLYWHEEL_LIVE_E2E=1 FLYWHEEL_LIVE_PROJECT=e2e-events \
//!     FLYWHEEL_LIVE_STATE_DIR=<doorbell state dir> \
//!     cargo test -p flywheel --test live_events_delivery -- --ignored --nocapture
//!
//! This test needs bd-rrlo2 (per-pin token mint) landed and an installed flywheel
//! and toron; the captain runs it with `bash scripts/e2e-live.sh --five`.

use flywheel_test_sandbox::TestSandbox;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const GATE: &str = "FLYWHEEL_LIVE_E2E";

fn gate() -> Option<(String, PathBuf)> {
    if std::env::var(GATE).ok().as_deref() != Some("1") {
        return None;
    }
    let project = std::env::var("FLYWHEEL_LIVE_PROJECT").ok()?;
    let state = std::env::var("FLYWHEEL_LIVE_STATE_DIR").ok()?;
    (!project.trim().is_empty() && !state.trim().is_empty())
        .then(|| (project, PathBuf::from(state)))
}

fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn json_field(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in keys {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::to_string)
}

fn poll(secs: u64, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// A retire guard: no matter which assertion panics, all three seats are closed.
struct Cleanup<'a> {
    sandbox: &'a TestSandbox,
    bin: &'a str,
    project: &'a str,
    pins: Vec<String>,
}
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        for pin in &self.pins {
            let _ = self
                .sandbox
                .command(self.bin)
                .args(["retire", pin, "--project", self.project, "--force-working"])
                .output();
        }
    }
}

#[test]
#[ignore = "gated live: FLYWHEEL_LIVE_E2E=1, bd-rrlo2 token mint, a throwaway rex session"]
fn one_addressed_mail_reaches_pi_opencode_and_claude_through_events_not_doorbell() {
    let Some((project, state_dir)) = gate() else {
        eprintln!(
            "bd-znczi live witness skipped: set FLYWHEEL_LIVE_E2E=1, FLYWHEEL_LIVE_PROJECT, FLYWHEEL_LIVE_STATE_DIR"
        );
        return;
    };
    let sandbox = TestSandbox::new();
    let bin = env!("CARGO_BIN_EXE_flywheel");
    let stamp = std::process::id();
    let tag = format!("znczi-{stamp}");

    // One throwaway session with all three harnesses. --model harness-default
    // injects no model; --escape/--yes keep this non-interactive.
    let spawned = sandbox
        .command(bin)
        .args([
            "spawn",
            &project,
            "--pi",
            "1",
            "--oc",
            "1",
            "--kind",
            "claude=1",
            "--model",
            "harness-default",
            "--escape",
            "--yes",
            "--retire-orphans",
            "--no-focus",
        ])
        .output()
        .expect("spawn three harness seats");
    assert!(spawned.status.success(), "spawn failed: {}", text(&spawned));

    let status = sandbox
        .command(bin)
        .args(["swarm", "status", &project, "--json"])
        .output()
        .expect("swarm status");
    assert!(status.status.success(), "status failed: {}", text(&status));
    let doc: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    let seats = doc
        .get("seats")
        .and_then(serde_json::Value::as_array)
        .expect("a seats array");
    assert_eq!(seats.len(), 3, "expected pi, opencode and claude seats");
    let pins: Vec<String> = seats
        .iter()
        .filter_map(|s| json_field(s, &["pin"]))
        .collect();
    assert_eq!(pins.len(), 3, "each seat must carry a pin");
    let _cleanup = Cleanup {
        sandbox: &sandbox,
        bin,
        project: &project,
        pins: pins.clone(),
    };

    let doorbell_journal_dir = state_dir.join("journals");
    for (i, pin) in pins.iter().enumerate() {
        let marker = format!("{tag}-{i}");

        // Addressed mail through the normal wake path.
        let sent = sandbox
            .command(bin)
            .args(["send", &project, &marker, "--to", pin, "--raw"])
            .output()
            .expect("send addressed mail");
        assert!(sent.status.success(), "send failed: {}", text(&sent));

        // One urgent mail: importance rides the toron envelope so the events
        // pump sees a second, steer-class wake for the same pin. The sender is
        // the registered live pin from the runner, not an invented agent.
        let sender = std::env::var("FLYWHEEL_LIVE_AS").unwrap_or_else(|_| tag.clone());
        let urgent = Command::new("toron")
            .args([
                "mail",
                "send",
                "--project",
                &project,
                "--as",
                &sender,
                "--to",
                pin,
                "--subject",
                &format!("urgent {marker}"),
                "--body",
                &format!("urgent {marker}"),
                "--importance",
                "urgent",
                "--wake",
                "tasked",
            ])
            .output()
            .expect("send urgent mail");
        assert!(
            urgent.status.success(),
            "urgent mail failed: {}",
            text(&urgent)
        );

        // (1) Events path active: the per-pin events journal got fetch/cursor rows.
        let events_journal = state_dir
            .join("doorbell")
            .join(format!("events-{pin}.jsonl"));
        let events_ok = poll(90, || {
            std::fs::read_to_string(&events_journal)
                .map(|s| s.lines().any(|l| l.contains("\"op\":\"fetch\"")))
                .unwrap_or(false)
        });
        assert!(
            events_ok,
            "{pin}: no events-journal rows — MCP Events is inert. With the per-pin token mint disabled the seat falls back to the DOORBELL (bd-rrlo2); this witness requires the events path."
        );

        // (2) No doorbell ring for this pin bearing our marker.
        let mut rings = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&doorbell_journal_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.to_string_lossy().contains("doorbell-journal-") {
                    continue;
                }
                if let Ok(body) = std::fs::read_to_string(&path) {
                    for line in body.lines() {
                        if line.contains(&format!("\"pin\":\"{pin}\"")) && line.contains(&marker) {
                            rings.push(line.to_string());
                        }
                    }
                }
            }
        }
        assert!(
            rings.is_empty(),
            "{pin}: the doorbell rang for {marker} — the seat fell back to the DOORBELL instead of the events path: {rings:?}"
        );
    }
}
