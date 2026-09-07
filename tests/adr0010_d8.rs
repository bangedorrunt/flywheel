// governed-by: ADR-0010 D8 (bd-d8e4) — corr-preserving resend + degraded receipts.
//! Receipts `owed → acked → resulted` gain `degraded(acked-unknown)`:
//! (1) transport-unknown prints `flywheel mail resend --corr <c>` (plain
//! re-run mints a new corr and duplicates); (2) post-enqueue bookkeeping
//! failure records `degraded` and never invites a resend; (3) uncertain
//! state prefers a rare duplicate wake over silence (D2 makes duplicates
//! safe).
//!
//! Test names: adr0010_d8_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d8`.

use toron_core::index::schema;
use toron_core::mailbox::Receipt;
use toron_core::mailbox::receipts::{DEGRADED_NOTE, ReceiptState, ReceiptStore, resend_guidance};
use toron_core::mailbox::send_tags::Wake;

const MESSAGING: &str = include_str!("../../toron/crates/toron-core/src/mailbox/messaging.rs");
const MAIN: &str = include_str!("../src/main.rs");

async fn test_store(tag: &str) -> (ReceiptStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{tag}.sqlite"));
    let pool = schema::connect(path.to_str().unwrap())
        .await
        .expect("connect");
    schema::migrate(&pool).await.expect("migrate");
    (ReceiptStore::new(pool), dir)
}

// (1) D8 degraded state: typed, persisted, and monotonic.
#[tokio::test]
async fn adr0010_d8_degraded_state_roundtrip_and_guards() {
    let (store, _dir) = test_store("degraded").await;

    assert_eq!(ReceiptState::Degraded.as_str(), "degraded");
    assert_eq!(
        ReceiptState::parse("degraded").expect("parse degraded"),
        ReceiptState::Degraded
    );

    let base = |state, updated_at| Receipt {
        corr: "C-8".into(),
        state,
        wake: Some(Wake::Tasked),
        due_at: None,
        result_path: None,
        updated_at,
    };

    // Roundtrip through sqlite.
    store
        .upsert(&base(ReceiptState::Degraded, 1))
        .await
        .expect("upsert degraded");
    let got = store.get("C-8").await.expect("get").expect("found");
    assert_eq!(got.state, ReceiptState::Degraded);

    // Degraded never downgrades to owed (never re-owes → never re-invites resend).
    store
        .upsert(&base(ReceiptState::Owed, 2))
        .await
        .expect("upsert owed");
    assert_eq!(
        store.get("C-8").await.expect("get").expect("found").state,
        ReceiptState::Degraded,
        "degraded must not be dragged back to owed"
    );

    // Resulted still wins over degraded (recovery path).
    store
        .set_resulted("C-8", Some("/tmp/out.json".to_string()))
        .await
        .expect("resulted");
    assert_eq!(
        store.get("C-8").await.expect("get").expect("found").state,
        ReceiptState::Resulted
    );

    // Acked never downgrades to degraded (an ack is strictly more information).
    store
        .upsert(&Receipt {
            corr: "C-9".into(),
            state: ReceiptState::Acked,
            wake: None,
            due_at: None,
            result_path: None,
            updated_at: 1,
        })
        .await
        .expect("seed acked");
    store
        .upsert(&Receipt {
            corr: "C-9".into(),
            state: ReceiptState::Degraded,
            wake: None,
            due_at: None,
            result_path: None,
            updated_at: 2,
        })
        .await
        .expect("upsert degraded over acked");
    assert_eq!(
        store.get("C-9").await.expect("get").expect("found").state,
        ReceiptState::Acked,
        "acked must not downgrade to degraded"
    );
}

// (2) The printed resend command is corr-preserving; the degraded note never
// invites a resend.
#[test]
fn adr0010_d8_resend_guidance_names_corr_preserving_resend() {
    let g = resend_guidance(Some("C-1"));
    assert!(
        g.contains("flywheel mail resend --corr C-1"),
        "transport-unknown guidance must name the corr-preserving resend command, got: {g}"
    );
    assert!(
        g.to_lowercase().contains("duplicat"),
        "guidance must call out that a plain re-run mints a new corr and duplicates, got: {g}"
    );
    let gn = resend_guidance(None);
    assert!(
        !gn.contains("resend --corr"),
        "no corr → no corr-preserving resend to name, got: {gn}"
    );
    assert!(
        gn.to_lowercase().contains("new corr"),
        "no-corr guidance must warn a re-run mints a new corr, got: {gn}"
    );
}

#[test]
fn adr0010_d8_degraded_note_never_invites_resend() {
    let n = DEGRADED_NOTE.to_lowercase();
    assert!(
        n.contains("do not resend") || n.contains("don't resend"),
        "degraded note must forbid a resend (a resend would duplicate), got: {DEGRADED_NOTE}"
    );
    assert!(
        n.contains("degraded"),
        "degraded note must name the degraded state, got: {DEGRADED_NOTE}"
    );
}

// (3) Wiring: the transport-unknown arm in send_message prints the resend
// guidance (and rings the wake anyway — duplicate over silence); the
// bookkeeping-failure arm records Degraded + the no-resend note.
#[test]
fn adr0010_d8_transport_unknown_arm_prints_resend_and_rings_wake() {
    assert!(
        MESSAGING.contains("resend_guidance"),
        "D8: send_message transport-unknown arm must print resend_guidance(corr)"
    );
    // Duplicate over silence: the wake ring must not be skipped when the
    // wrap may not have landed (D2 makes the duplicate safe).
    assert!(
        MESSAGING.contains("duplicate over silence"),
        "D8: the transport-unknown arm must document ringing the wake anyway (duplicate over silence)"
    );
}

#[test]
fn adr0010_d8_bookkeeping_failure_records_degraded() {
    assert!(
        MESSAGING.contains("ReceiptState::Degraded"),
        "D8: post-enqueue bookkeeping failure must record ReceiptState::Degraded"
    );
    assert!(
        MESSAGING.contains("DEGRADED_NOTE"),
        "D8: the degraded record must print DEGRADED_NOTE (no resend invitation)"
    );
}

// (4) Flywheel sweep-side consumption: the receipts bridge maps `degraded`
// (today it silently drops unknown states — the exact silence D8 forbids).
#[test]
fn adr0010_d8_upsert_workspace_receipt_maps_degraded() {
    assert!(
        MAIN.contains("\"degraded\" => toron_core::mailbox::receipts::ReceiptState::Degraded"),
        "D8: upsert_workspace_receipt must map the degraded state (silent drop = the failure D8 forbids)"
    );
}

// (5) CLI error path: `flywheel mail resend --corr <c>` exists and refuses
// the states where a resend would duplicate.
#[test]
fn adr0010_d8_mail_resend_command_refuses_duplicate_shaped_states() {
    assert!(
        MAIN.contains("Resend {"),
        "D8: MailCmd must carry a Resend variant (the printed command must exist)"
    );
    assert!(
        MAIN.contains("already resulted") && MAIN.contains("resend"),
        "D8: mail resend must refuse a resulted corr (resend would duplicate)"
    );
}

// (6) Runtime triad, producer side: corr-keyed send is idempotent — an
// identical re-send is the Duplicate path (no new event, no second wake,
// receipt untouched), so a duplicate wake through D2 is a no-op.
#[tokio::test]
async fn adr0010_d8_corr_send_duplicate_is_noop_and_receipt_survives() {
    use std::sync::Arc;
    use toron_core::bus::in_process::{AuthPolicy, InProcessBus, InProcessBusConfig};
    use toron_core::mailbox::Mailbox;
    use toron_core::mailbox::messaging::SendMessage;
    use toron_core::nostr::Tag;
    use toron_core::nostr::identity::AgentProfile;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = schema::connect(dir.path().join("i.sqlite").to_str().unwrap())
        .await
        .expect("connect");
    schema::migrate(&pool).await.expect("migrate");
    let bus = InProcessBus::open(
        pool.clone(),
        InProcessBusConfig {
            auth: AuthPolicy::AcceptAll,
            allowlist: vec![],
            channel_capacity: 256,
            detailed_stage_logging: false,
        },
    )
    .await
    .expect("bus");
    let mailbox = Mailbox::new(Arc::new(bus), dir.path().to_path_buf(), None)
        .with_agent_passphrase("toron-dev")
        .with_index_pool(pool.clone());
    mailbox
        .register_agent("d8", &AgentProfile::new("RedCorr"))
        .await
        .expect("register sender");
    mailbox
        .register_agent("d8", &AgentProfile::new("BlueCorr"))
        .await
        .expect("register target");

    let msg = || SendMessage {
        project_slug: "d8".into(),
        sender_name: "RedCorr".into(),
        to: vec!["BlueCorr".into()],
        subject: "d8".into(),
        body_md: "triad".into(),
        extra_tags: vec![
            Tag::parse(["corr", "FM-D8"]).expect("corr tag"),
            Tag::parse(["wake", "tasked"]).expect("wake tag"),
        ],
        ..Default::default()
    };

    let first = mailbox.send_message(&msg()).await.expect("first send");
    assert_eq!(first.count, 1, "one delivery");

    // Receipt recorded owed, corr-keyed.
    let store = mailbox.receipt_store().expect("receipt store");
    let r = store.get("FM-D8").await.expect("get").expect("receipt");
    assert_eq!(r.state, ReceiptState::Owed);

    // Identical re-send (the crash-resume path): idempotent claim replays the
    // FIRST delivery set — no new event, no second wake, receipt untouched.
    let second = mailbox.send_message(&msg()).await.expect("re-send");
    assert_eq!(second.count, 1, "duplicate replays, does not re-emit");
    assert_eq!(
        second.deliveries[0].payload.event_id, first.deliveries[0].payload.event_id,
        "corr-preserving re-send converges on the first rumor"
    );
    assert_eq!(
        store
            .get("FM-D8")
            .await
            .expect("get")
            .expect("receipt")
            .state,
        ReceiptState::Owed,
        "receipt state untouched by the duplicate"
    );

    // Resulted is terminal even against a re-send's owed upsert.
    store.set_resulted("FM-D8", None).await.expect("resulted");
    let _ = mailbox
        .send_message(&msg())
        .await
        .expect("post-result re-send");
    assert_eq!(
        store
            .get("FM-D8")
            .await
            .expect("get")
            .expect("receipt")
            .state,
        ReceiptState::Resulted,
        "resulted never downgrades (resend safety at the receipt level)"
    );
}
