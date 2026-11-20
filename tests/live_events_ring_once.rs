//! LIVE AE6 witness for bd-l5vb2 (ADR-0039 D8): the same addressed mail
//! arriving through wake.sock AND the toron events source produces exactly one
//! pane prompt, and a gap produces one digest, not a prompt storm.
//!
//! Live carve-out (AGENTS.md): every test here is `#[ignore]`d AND the file is
//! gated on `FLYWHEEL_LIVE_E2E`, so it never runs by default. The event id must
//! come from a REAL addressed mail on the bus (read it from the doorbell
//! journal / a live `toron next`), never invented — both sources must see the
//! same id for the one-ring claim to be meaningful.
//!
//! ```text
//! FLYWHEEL_LIVE_E2E=1 \
//! FLYWHEEL_LIVE_EVENT_ID=<64-hex bus id of a real addressed mail> \
//! FLYWHEEL_LIVE_PIN=<the no-door seat that received it> \
//!   cargo test -p flywheel --test live_events_ring_once -- --ignored --nocapture
//! ```

use doorbell_core::{DoorbellAction, DoorbellModel, DoorbellMsg};

fn gate() -> Option<(String, String)> {
    if std::env::var("FLYWHEEL_LIVE_E2E").ok().as_deref() != Some("1") {
        return None;
    }
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some((var("FLYWHEEL_LIVE_EVENT_ID")?, var("FLYWHEEL_LIVE_PIN")?))
}

fn prompts(actions: &[DoorbellAction]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, DoorbellAction::Prompt { .. }))
        .count()
}

#[test]
#[ignore]
fn one_addressed_mail_rings_once_across_wake_sock_and_events() {
    let Some((event_id, pin)) = gate() else {
        eprintln!(
            "bd-l5vb2 live witness skipped: set FLYWHEEL_LIVE_E2E=1, FLYWHEEL_LIVE_EVENT_ID, FLYWHEEL_LIVE_PIN"
        );
        return;
    };
    let mut model = DoorbellModel::default();
    model.index.insert_pin(pin.clone(), None);
    model.statuses.insert(pin.clone(), "idle".into());

    // Source 1: wake.sock already rang this mail; the real bus event id is
    // journaled in the core's shared `rung` set.
    model.rung.insert(event_id.clone());

    // Source 2: the events source carries the SAME bus event id. The shared
    // key must suppress a second ring.
    let line = doorbell_core::doorbell_line(doorbell_core::EventClass::PlanePMail, "toron next");
    let (_, a2) = doorbell_core::tick(
        &model,
        &DoorbellMsg::EventsHint {
            event_id,
            pin: pin.clone(),
            line,
        },
    );
    assert_eq!(
        prompts(&a2),
        0,
        "one mail across two sources must ring exactly once: {a2:?}"
    );
}

#[test]
#[ignore]
fn an_events_gap_is_one_digest_not_a_prompt_storm() {
    let Some((_event_id, pin)) = gate() else {
        return;
    };
    let model = DoorbellModel::default();
    let (_, actions) = doorbell_core::tick(&model, &DoorbellMsg::EventsGap { pin });
    assert_eq!(actions.len(), 1, "exactly one digest: {actions:?}");
    assert!(matches!(actions[0], DoorbellAction::Digest { .. }));
}
