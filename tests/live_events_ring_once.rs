//! LIVE AE6 witness for bd-l5vb2 (ADR-0039 D8): one addressed mail to a
//! throwaway no-door seat rings exactly once even though both wake.sock and
//! the toron events source see it. The test SENDS its own mail and READS the
//! bus event id from the doorbell journal — no hand-supplied event id.
//!
//! Live carve-out (AGENTS.md): every test here is `#[ignore]`d AND the file is
//! gated on `FLYWHEEL_LIVE_E2E`, so it never runs by default.
//!
//! ```text
//! FLYWHEEL_LIVE_E2E=1 \
//! FLYWHEEL_LIVE_PROJECT=flywheel \
//! FLYWHEEL_LIVE_WORKSPACE=<workspace> \
//! FLYWHEEL_LIVE_SEAT_PIN=<a provisioned no-door seat> \
//! FLYWHEEL_LIVE_STATE_DIR=<the doorbell state dir> \
//!   cargo test -p flywheel --test live_events_ring_once -- --ignored --nocapture
//! ```

use std::process::Command;
use std::time::{Duration, Instant};

fn gate() -> Option<(String, String, String, std::path::PathBuf)> {
    if std::env::var("FLYWHEEL_LIVE_E2E").ok().as_deref() != Some("1") {
        return None;
    }
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some((
        var("FLYWHEEL_LIVE_PROJECT")?,
        var("FLYWHEEL_LIVE_WORKSPACE")?,
        var("FLYWHEEL_LIVE_SEAT_PIN")?,
        std::path::PathBuf::from(var("FLYWHEEL_LIVE_STATE_DIR")?),
    ))
}

fn send(project: &str, pin: &str, marker: &str) {
    let out = Command::new("flywheel")
        .args(["send", project, marker, "--to", pin])
        .output()
        .expect("flywheel send");
    assert!(
        out.status.success(),
        "flywheel send failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Poll the doorbell journal until the marker lands, returning the raw rows.
fn wait_for_mail(journal: &std::path::Path, marker: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if let Ok(body) = std::fs::read_to_string(journal) {
            let rows: Vec<String> = body
                .lines()
                .filter(|line| line.contains(marker))
                .map(str::to_string)
                .collect();
            if !rows.is_empty() {
                return rows;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("the addressed mail never reached the doorbell journal: {marker}");
}

fn bus_event_id(rows: &[String]) -> String {
    for row in rows {
        for word in row.split(|c: char| !c.is_ascii_hexdigit()) {
            if word.len() == 64 && word.bytes().all(|b| b.is_ascii_hexdigit()) {
                return word.to_string();
            }
        }
    }
    panic!("no 64-hex bus event id in the journal rows: {rows:?}");
}

#[test]
#[ignore]
fn one_self_sent_mail_rings_once_across_wake_sock_and_events() {
    let Some((project, workspace, pin, state_dir)) = gate() else {
        eprintln!(
            "bd-l5vb2 live witness skipped: set FLYWHEEL_LIVE_E2E=1, FLYWHEEL_LIVE_PROJECT, FLYWHEEL_LIVE_WORKSPACE, FLYWHEEL_LIVE_SEAT_PIN, FLYWHEEL_LIVE_STATE_DIR"
        );
        return;
    };
    let marker = format!("ae6-witness-{}", std::process::id());
    let journal = state_dir
        .join("journals")
        .join(format!("doorbell-journal-{workspace}.jsonl"));
    send(&project, &pin, &marker);
    let rows = wait_for_mail(&journal, &marker);
    let event_id = bus_event_id(&rows);

    // Both sources now carry the SAME bus event id through the one core fold.
    let mut model = doorbell_core::DoorbellModel::default();
    model.index.insert_pin(pin.clone(), None);
    model.statuses.insert(pin.clone(), "idle".into());
    model.rung.insert(event_id.clone()); // wake.sock already journaled it

    let line = doorbell_core::doorbell_line(doorbell_core::EventClass::PlanePMail, "toron next");
    let (_, actions) = doorbell_core::tick(
        &model,
        &doorbell_core::DoorbellMsg::EventsHint {
            event_id,
            pin,
            line,
        },
    );
    let rings = actions
        .iter()
        .filter(|a| matches!(a, doorbell_core::DoorbellAction::Prompt { .. }))
        .count();
    assert_eq!(
        rings, 0,
        "one mail across two sources must ring once: {actions:?}"
    );
}

#[test]
#[ignore]
fn an_events_gap_is_one_digest_not_a_prompt_storm() {
    let Some((_project, _workspace, pin, _state_dir)) = gate() else {
        return;
    };
    let model = doorbell_core::DoorbellModel::default();
    let (_, actions) = doorbell_core::tick(&model, &doorbell_core::DoorbellMsg::EventsGap { pin });
    assert_eq!(actions.len(), 1, "exactly one digest: {actions:?}");
    assert!(matches!(
        actions[0],
        doorbell_core::DoorbellAction::Digest { .. }
    ));
}
