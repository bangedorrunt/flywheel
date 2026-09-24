// governed-by: bd-z7w0 — degraded/fallback marker survey ledger.
//! Every degraded/fallback marker, its disposition, and when it fires.
//! Reachable markers are KEPT and their firing condition is pinned here;
//! deletions happen only with an unreachability proof, one commit each.
//! The Recover operator stays (rule). Functional reachability tests live
//! beside the code as degraded_audit_* unit tests; this file pins the
//! ledger structurally so a deletion without its proof fails loudly.
//!
//! Test anchor: `mbx test degraded_audit`.

const DOORBELL: &str = include_str!("../src/doorbell.rs");
const DAEMON: &str = include_str!("../src/daemon.rs");
const MAIN: &str = include_str!("../src/main.rs");
const PANE: &str = include_str!("../src/pane.rs");
const AGENTS: &str = include_str!("../AGENTS.md");

/// The survey ledger: (marker, disposition, fires-when). A marker removed
/// from the code without updating this ledger fails here first.
const LEDGER: &[(&str, &str, &str)] = &[
    (
        "ensure Degraded arm",
        "KEEP",
        "FLYWHEEL_NO_DOORBELL_ENSURE=1, missing binary, or failed spawn",
    ),
    (
        "resolve_host Embedded (TakeLease)",
        "KEEP",
        "configured daemon unreachable, or quiet default port",
    ),
    (
        "resolve_host Embedded (RefuseDoubleRun)",
        "KEEP",
        "live foreign lease holds the workspace",
    ),
    (
        "bypassed-no-effect-runner",
        "KEEP",
        "dry arms: run without --execute",
    ),
    (
        "bypassed-sink-uncovered",
        "KEEP",
        "remote [mail.toron] url: mailbox leaves the wire cannot corr",
    ),
    (
        "unroutable pin Digest",
        "KEEP",
        "subscribed pin with no pane: digest, never a parked queue",
    ),
    (
        "Recover operator",
        "STAYS",
        "rule: supervised recovery, never deleted",
    ),
];

#[test]
fn degraded_audit_ledger_markers_present_in_code_or_docs() {
    for (marker, disposition, fires) in LEDGER {
        let needle = match *marker {
            "ensure Degraded arm" => "Degraded",
            "resolve_host Embedded (TakeLease)" => "embedded degraded mode",
            "resolve_host Embedded (RefuseDoubleRun)" => "refusing second loop",
            "bypassed-no-effect-runner" => "bypassed-no-effect-runner",
            "bypassed-sink-uncovered" => "bypassed-sink-uncovered",
            "unroutable pin Digest" => "Digest",
            "Recover operator" => "Recover { project",
            _ => unreachable!("ledger marker without a probe: {marker}"),
        };
        let haystacks = [DOORBELL, DAEMON, MAIN, PANE, AGENTS];
        assert!(
            haystacks.iter().any(|h| h.contains(needle)),
            "{disposition} marker vanished without proof: {marker} (fires: {fires})"
        );
    }
}

#[test]
fn degraded_audit_no_deletion_without_proof() {
    // The ledger IS the proof registry: 7 rows, each with a disposition
    // and a firing condition. An empty ledger (or a row without a
    // condition) means the survey was skipped, not completed.
    assert_eq!(LEDGER.len(), 7, "survey covers all 7 markers");
    for (marker, disposition, fires) in LEDGER {
        assert!(
            matches!(*disposition, "KEEP" | "STAYS"),
            "only proven-dead branches delete (none proven yet): {marker}"
        );
        assert!(!fires.is_empty(), "every row names when it fires: {marker}");
    }
}
