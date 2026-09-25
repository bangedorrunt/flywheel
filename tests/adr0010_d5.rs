// governed-by: ADR-0010 D5 (bd-65he) — Wave 2 gate.
//! Classify wakes in tick; doorbell afk digest (gated on D0 + D3).
//!
//! Test names: adr0010_d5_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d5`.
//! Behavior tests for the pure core live in src/workspace.rs #[cfg(test)]
//! mod adr0010_d5_tests (same filter); this file pins the source contract.

const WORKSPACE: &str = include_str!("../src/workspace.rs");
const MAIN: &str = include_str!("../src/main.rs");
const DURABLE: &str = include_str!("../src/durable.rs");

/// D5 structural gate: D0 closed — poll_mail_digests is not a Msg
/// producer anywhere in the live path.
#[test]
fn adr0010_d5_d0_gate_no_second_wake_producer() {
    assert!(
        !DURABLE.contains("async fn poll_mail_digests"),
        "D5 boundary (bd-65he): D0 gate broken — poll_mail_digests producer back in durable.rs"
    );
    assert!(
        !DURABLE.contains("register(poll_mail_digests)"),
        "D5 boundary (bd-65he): D0 gate broken — poll_mail_digests re-registered"
    );
}

/// D5 classes: benign spends zero prompts, routine journals+counts,
/// captain-relevant queues for digest, urgent prompts immediately.
#[test]
fn adr0010_d5_wake_classes_present() {
    for class in ["Benign", "Routine", "CaptainRelevant", "Urgent"] {
        assert!(
            WORKSPACE.contains(class),
            "D5 (bd-65he): workspace.rs must define WakeClass::{class}"
        );
    }
}

/// D5 digest path: queue in tick, Digest cmd, afk render with outcomes
/// first, bead ids kept, exact commands last.
#[test]
fn adr0010_d5_digest_path_present() {
    // 7d36bf8 moved the digest queue into `WakeModel` (`wake.digests`); the
    // tick still folds `Cmd::Digest` rows into `DigestEntry` and renders
    // through `render_digest`.
    assert!(
        WORKSPACE.contains(".digests") && WORKSPACE.contains("Cmd::Digest"),
        "D5 (bd-65he): tick must queue captain-relevant wakes for digest"
    );
    assert!(
        WORKSPACE.contains("Digest"),
        "D5 (bd-65he): workspace.rs must define the Digest cmd + render"
    );
    assert!(
        MAIN.contains("Afk"),
        "D5 (bd-65he): flywheel doorbell afk CLI must exist"
    );
}

/// D5 batching knobs: FLYWHEEL_DIGEST_SECS=300 default + threshold N.
#[test]
fn adr0010_d5_digest_knobs_present() {
    assert!(
        WORKSPACE.contains("FLYWHEEL_DIGEST_SECS"),
        "D5 (bd-65he): digest batching honors FLYWHEEL_DIGEST_SECS (default 300)"
    );
    assert!(
        WORKSPACE.contains("300"),
        "D5 (bd-65he): default digest interval 300s must appear"
    );
}

/// D5 absorb gate: D3 Busy exists so urgent has an authority to consult
/// (classification never invents its own presence read).
#[test]
fn adr0010_d5_busy_absorb_gate_present() {
    assert!(
        WORKSPACE.contains("Busy"),
        "D5 (bd-65he): D3 Busy verdict must exist for urgent absorb"
    );
}
