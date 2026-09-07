// governed-by: ADR-0010 D10 (bd-67qg) — Wave 2 gate.
//! One fleet snapshot contract + `flywheel helm`.
//!
//! Test names: adr0010_d10_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d10`.
//! Behavior tests for the pure core live in src/snapshot.rs #[cfg(test)]
//! (same filter); this file pins the source contract.

const MAIN: &str = include_str!("../src/main.rs");

/// D10 snapshot contract: versioned schema assembled once, consumed by
/// status / view / watch / steer_brief.
#[test]
fn adr0010_d10_snapshot_contract_present() {
    assert!(
        MAIN.contains("flywheel-snapshot.v1"),
        "D10 (bd-67qg): snapshot schema flywheel-snapshot.v1 must exist"
    );
    for surface in [
        "fn cmd_status",
        "fn cmd_view",
        "fn cmd_watch",
        "fn steer_briefing_context",
    ] {
        assert!(
            MAIN.contains(surface),
            "D10 (bd-67qg): surface {surface} must exist to consume the snapshot"
        );
    }
    assert!(
        MAIN.contains("snapshot::assemble"),
        "D10 (bd-67qg): all four surfaces render from one snapshot call"
    );
}

/// D10 helm: the one session-start drain — always prints OPEN DECISIONS,
/// UNREAD STATUS, RECORD DIVERGENCE, even on an empty queue.
#[test]
fn adr0010_d10_helm_drain_present() {
    assert!(
        MAIN.contains("Helm") || MAIN.contains("fn cmd_helm"),
        "D10 (bd-67qg): flywheel helm command must exist"
    );
    for section in ["OPEN DECISIONS", "UNREAD STATUS", "RECORD DIVERGENCE"] {
        assert!(
            MAIN.contains(section),
            "D10 (bd-67qg): helm must print {section} even when empty"
        );
    }
}

/// D10 robot surface: capabilities entry + validating robot JSON.
#[test]
fn adr0010_d10_robot_surface_present() {
    assert!(
        MAIN.contains("helm"),
        "D10 (bd-67qg): robot_capabilities must list helm/snapshot"
    );
}
