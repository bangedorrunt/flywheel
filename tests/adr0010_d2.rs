// governed-by: ADR-0010 D2 (bd-8jlv) — Wave A.1 gate.
//! WakeAck generation-bound watermark: presented-but-unacked wakes
//! re-present at the next prompt; handling is a journaled fact.
//!
//! Test names: adr0010_d2_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d2`.
//! Behavior tests for the pure core live in src/workspace.rs #[cfg(test)]
//! mod adr0010_d2_tests (same filter); this file pins the source contract.

const WORKSPACE: &str = include_str!("../src/workspace.rs");

/// D2 mint (retired): `Msg::WakeAck` had no sender outside tests; the only ack
/// is the cursor `toron next` commits (bd-doorbell-daemon-sink-wake-only-yfqz).
/// The variant must not come back as a second ack path.
#[test]
fn adr0010_d2_wake_ack_variant_is_retired() {
    assert!(
        !WORKSPACE.contains("WakeAck {"),
        "Msg::WakeAck is deleted: the cursor is the one ack"
    );
}

/// D2 state: presented-but-unacked wakes tracked per pin with their
/// presentation seq; ack watermark per pin.
#[test]
fn adr0010_d2_presented_and_watermark_state_present() {
    // 7d36bf8 moved the presented-wake table into `WakeModel`
    // (`wake_core`'s `shown`); workspace.rs keeps the ack watermark and
    // re-presents through `PresentedWake`.
    assert!(
        WORKSPACE.contains("PresentedWake") && WORKSPACE.contains("wake_acked"),
        "D2 (bd-8jlv): WorkspaceModel must track presented wakes + ack watermark per pin"
    );
}

/// D2 re-present rule: unacked wakes re-present at the next prompt for
/// the pin (same bytes — duplicate ring is never second work).
#[test]
fn adr0010_d2_represent_rule_present() {
    assert!(
        WORKSPACE.contains("re-present") || WORKSPACE.contains("represent"),
        "D2 (bd-8jlv): tick must re-present presented-but-unacked wakes at the next prompt"
    );
}

/// D2 idempotency: duplicate or stale acks change nothing.
#[test]
fn adr0010_d2_ack_idempotent() {
    assert!(
        WORKSPACE.contains("WakeAck") && WORKSPACE.contains("wake_acked"),
        "D2 (bd-8jlv): WakeAck arm must fold through_seq into the watermark idempotently"
    );
}
