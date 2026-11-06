// governed-by: ADR-0010 D1 (bd-w98c) — Wave A.1 gate.
//! Doorbell payload is toron mail; re-ring ladder plus fire-and-forget skip.
//!
//! Test names: adr0010_d1_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d1`.
//! Behavior tests for the pure ladder core live in src/workspace.rs
//! #[cfg(test)] mod (same filter); this file pins the source contract.

const WORKSPACE: &str = include_str!("../src/workspace.rs");
const TPL: &str = include_str!("../integrations/peer-aware.md.tpl");
const DOORBELL: &str = include_str!("../src/doorbell.rs");

/// main.rs plus the cmd/* bodies it was split into (the eviction moved
/// cmd_send there; a source contract that used to grep main.rs must follow).
fn main_plus_cmd() -> String {
    fn visit(dir: &std::path::Path, out: &mut String) {
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .expect("src readable")
            .map(|e| e.expect("dir entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                visit(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push_str(&std::fs::read_to_string(&path).expect("read src"));
                out.push('\n');
            }
        }
    }
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = std::fs::read_to_string(src.join("main.rs")).expect("main.rs");
    out.push('\n');
    visit(&src.join("cmd"), &mut out);
    out
}

/// D1 ladder knobs: grace 90s, max 3 rings — then Dead→respawn, Unknown→digest.
#[test]
fn adr0010_d1_ladder_knobs_present() {
    assert!(
        WORKSPACE.contains("FLYWHEEL_DOORBELL_GRACE_SECS"),
        "D1 (bd-w98c): workspace.rs must read FLYWHEEL_DOORBELL_GRACE_SECS (default 90)"
    );
    assert!(
        WORKSPACE.contains("FLYWHEEL_DOORBELL_RING_MAX"),
        "D1 (bd-w98c): workspace.rs must read FLYWHEEL_DOORBELL_RING_MAX (default 3)"
    );
    assert!(
        WORKSPACE.contains("90") && WORKSPACE.contains('3'),
        "D1 (bd-w98c): ladder defaults 90s grace / 3 rings must appear in workspace.rs"
    );
}

/// D1 shrunk prompt: `[doorbell] inbox: <n> unacked for <pin>; latest corr=<c>; read + ack`.
#[test]
fn adr0010_d1_doorbell_line_ctor_present() {
    assert!(
        WORKSPACE.contains("fn doorbell_mail_line"),
        "D1 (bd-w98c): workspace.rs must define doorbell_mail_line(unacked, pin, corr)"
    );
    assert!(
        WORKSPACE.contains("unacked for"),
        "D1 (bd-w98c): doorbell line must render `[doorbell] inbox: <n> unacked for <pin>; latest corr=<c>; read + ack`"
    );
    let main = main_plus_cmd();
    assert!(
        main.contains("delivery_id_of") && main.contains("delivery_id"),
        "D1 (bd-w98c): cmd_send must bind --fire-and-forget <id> to the delivery record (ladder skip key)"
    );
}

/// D1 fire-and-forget bind: `--fire-and-forget <delivery-id>` caller-supplied
/// id; re-enqueue idempotent; acked corrs skip the ladder.
#[test]
fn adr0010_d1_fire_and_forget_id_bound_to_ladder() {
    let main = main_plus_cmd();
    assert!(
        main.contains("delivery-id") || main.contains("delivery_id"),
        "D1 (bd-w98c): --fire-and-forget must accept a caller-supplied <delivery-id>"
    );
    assert!(
        WORKSPACE.contains("fire_and_forget") || WORKSPACE.contains("fire-and-forget"),
        "D1 (bd-w98c): the ladder must skip fire-and-forget deliveries (never climbs)"
    );
}

/// D1 grace-window ladder is the escalation path: AckSlaTick advances
/// ladder entries (ring per window, Dead→respawn, Unknown→digest).
#[test]
fn adr0010_d1_flat_digest_replaced_by_ladder() {
    assert!(
        WORKSPACE.contains("ReringDecision::RecoverDead")
            && WORKSPACE.contains("ReringDecision::DigestUnknown"),
        "D1 (bd-w98c): workspace tick must escalate exhausted ladder entries (Dead→respawn, Unknown→digest)"
    );
    assert!(
        WORKSPACE.contains("Msg::AckSlaTick") && WORKSPACE.contains("next.wake.ladder_step"),
        "D1 (bd-w98c): the AckSlaTick beat must advance ladder entries in workspace::tick"
    );
    let _ = DOORBELL;
}

/// D1 handler duty: fetch_inbox in rumor order, acknowledge_message /
/// mark_message_read after handling; ID DISCIPLINE lines preserved.
#[test]
fn adr0010_d1_template_ack_duty_present() {
    assert!(
        TPL.contains("fetch_inbox") || TPL.contains("inbox --since"),
        "D1 (bd-w98c): template must teach fetch_inbox pull on doorbell"
    );
    assert!(
        TPL.contains("acknowledge_message") || TPL.contains("mark_message_read"),
        "D1 (bd-w98c): template must teach acknowledge_message / mark_message_read (ack is the handling fact)"
    );
    assert!(
        TPL.contains("ID DISCIPLINE"),
        "D1 (bd-w98c): template must preserve the ID DISCIPLINE lines (Lane-2 shared)"
    );
}

/// D1 raw rule: `--raw` without id rings once and does not escalate.
#[test]
fn adr0010_d1_raw_rings_once_no_escalate() {
    assert!(
        WORKSPACE.contains("raw") && WORKSPACE.contains("ReringDecision"),
        "D1 (bd-w98c): ladder must model raw-without-id as ring-once, never escalate"
    );
}
