// governed-by: ADR-0010 D9 (bd-l0fz) — structural done-hold via the bead tag.
//! Dispatch-guard done-hold stops matching a subject ending `] done`.
//! `toron mail send --resolve-bead bd-###` stamps `bead=<id>` (corr-linked);
//! the guard matches the TAG. Subject stays decoration for humans. Clean
//! cutover — no dual-match shim.
//!
//! AC: tag present + arbitrary subject → hold fires; tag absent + `] done`
//! subject → hold does NOT fire.
//!
//! Test names: adr0010_d9_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d9`.

use toron_core::index::schema;

const SEND_TAGS: &str = include_str!("../../toron/crates/toron-core/src/mailbox/send_tags.rs");
const DURABLE: &str = include_str!("../src/durable.rs");
const MESSAGING: &str = include_str!("../../toron/crates/toron-core/src/mailbox/messaging.rs");
const TORON_CLI: &str = include_str!("../../toron/crates/toron-cli/src/cli.rs");
const TORON_ROBOT: &str = include_str!("../../toron/crates/toron-cli/src/robot.rs");
const MAIN: &str = include_str!("../src/main.rs");
const SURFACE: &str = include_str!("../../toron/crates/toron-core/src/mailbox/surface.rs");

// (1) Structural cutover: subject-substring matching is DELETED, not shimmed.
#[test]
fn adr0010_d9_subject_done_deleted_no_dual_match() {
    assert!(
        !DURABLE.contains("fn subject_done"),
        "D9: subject_done must be deleted — the bead tag is the hold (subtract-before-you-add)"
    );
    assert!(
        !DURABLE.contains("ends_with(\"] done\")"),
        "D9: no `] done` subject-substring match may survive (no dual-match shim)"
    );
}

#[test]
fn adr0010_d9_classify_takes_the_bead_tag() {
    assert!(
        DURABLE.contains("bead: Option<&str>"),
        "D9: classify_guard_signal must consume the projected bead tag"
    );
    assert!(
        DURABLE.contains("GuardSignalKind::Done"),
        "D9: Done signal remains the hold vocabulary"
    );
}

// (2) toron send-tags vocabulary: `bead=<id>` is typed (non-empty) and rides
// the rumor; the CLI flag stamps it.
#[test]
fn adr0010_d9_send_tags_vocabulary_has_bead() {
    assert!(
        SEND_TAGS.contains("\"bead\" =>"),
        "D9: SendTags::parse must accept the bead tag"
    );
    assert!(
        SEND_TAGS.contains("pub bead:"),
        "D9: SendTags must carry the bead field (typed, not free strings)"
    );
    assert!(
        TORON_CLI.contains("resolve_bead"),
        "D9: toron mail send must expose --resolve-bead (CLI-only flag)"
    );
    assert!(
        TORON_ROBOT.contains("[\"bead\""),
        "D9: the CLI stamps the bead tag into extra_tags"
    );
}

// (3) Projection: the rumor's bead tag reaches inbox rows; the surface
// MailItem carries it to the guard scan.
#[test]
fn adr0010_d9_inbox_projection_carries_bead() {
    assert!(
        MESSAGING.contains("bead: tag_value(tags, \"bead\")"),
        "D9: inbox_from must project the rumor's bead tag"
    );
    assert!(
        SURFACE.contains("pub bead:"),
        "D9: surface MailItem must carry the bead to the guard scan"
    );
    assert!(
        MAIN.contains("m.bead"),
        "D9: __dispatch-guard scan must pass the row's bead into classify"
    );
}

// (4) Runtime AC, end-to-end over a real mailbox:
// tag present + arbitrary subject → Done; tag absent + `] done` → None.
#[tokio::test]
async fn adr0010_d9_bead_tag_is_the_hold_subject_is_decoration() {
    use std::sync::Arc;
    use toron_core::bus::in_process::{AuthPolicy, InProcessBus, InProcessBusConfig};
    use toron_core::mailbox::Mailbox;
    use toron_core::mailbox::messaging::SendMessage;
    use toron_core::nostr::Tag;
    use toron_core::nostr::identity::AgentProfile;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = schema::connect(dir.path().join("d9.sqlite").to_str().unwrap())
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
        .register_agent("d9", &AgentProfile::new("RedTag"))
        .await
        .expect("register sender");
    mailbox
        .register_agent("d9", &AgentProfile::new("BlueTag"))
        .await
        .expect("register target");

    let send = |subject: &str, bead: Option<&str>| {
        let mut extra = vec![Tag::parse(["wake", "tasked"]).expect("wake tag")];
        if let Some(b) = bead {
            extra.push(Tag::parse(["bead", b]).expect("bead tag"));
        }
        SendMessage {
            project_slug: "d9".into(),
            sender_name: "RedTag".into(),
            to: vec!["BlueTag".into()],
            subject: subject.into(),
            body_md: "report body".into(),
            thread_id: Some("bd-l0fz".into()),
            extra_tags: extra,
            ..Default::default()
        }
    };

    // Tag present + ARBITRARY subject → the hold fires (bead projects).
    mailbox
        .send_message(&send("shipped and verified", Some("bd-l0fz")))
        .await
        .expect("tagged send");
    // Tag absent + `] done` subject → the hold does NOT fire.
    mailbox
        .send_message(&send("[bd-l0fz] done", None))
        .await
        .expect("untagged send");

    let rows = mailbox
        .fetch_inbox(&toron_core::mailbox::messaging::FetchInbox {
            project_slug: "d9".into(),
            agent_name: "BlueTag".into(),
            unread_only: false,
            limit: Some(10),
            include_bodies: true,
            ..Default::default()
        })
        .await
        .expect("inbox");
    let tagged: Option<_> = rows.iter().find(|r| r.subject == "shipped and verified");
    let untagged: Option<_> = rows.iter().find(|r| r.subject.ends_with("] done"));
    assert_eq!(
        tagged.and_then(|r| r.bead.as_deref()),
        Some("bd-l0fz"),
        "tag present + arbitrary subject → bead projects → hold fires"
    );
    assert!(
        untagged.is_some(),
        "the `] done` subject row must exist (it is NOT collapsed away)"
    );
    assert_eq!(
        untagged.and_then(|r| r.bead.as_deref()),
        None,
        "tag absent + `] done` subject → hold does NOT fire (subject is decoration)"
    );
}
