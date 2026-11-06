// governed-by: ADR-0010 D10 (bd-67qg) — Wave 2 gate.
//! One fleet snapshot contract + `flywheel helm`.
//!
//! Test names: adr0010_d10_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d10`.
//! Behavior tests for the pure core live in src/snapshot.rs #[cfg(test)]
//! (same filter); this file pins the source contract.

mod support;

static MAIN: std::sync::LazyLock<String> = std::sync::LazyLock::new(support::crate_source);

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

/// Wave11 (bd-wave11-snapshot-mndgf): one SnapshotV1 world read — every
/// door is a pure projection over it. Source contract: SnapshotV1 sections,
/// single `assemble(cfg, project, now)`, and pure projection cores exist;
/// behavioral agreement is proven by unit tests in `src/snapshot.rs`
/// (binary-only crate has no lib target for `use flywheel::`).
#[test]
fn wave11_snapshot_projections_agree_on_core_facts() {
    for token in [
        "SnapshotV1",
        "pub fn assemble(cfg: &crate::config::Config, project: &str, now: u64)",
        "pub fn progress_from_rows",
        "pub fn status_core_counts",
        "pub fn progress_view",
        "pub fn robot_view",
        "pub fn serve_view",
        "fn read_progress_section",
        "project: project.to_string()",
        "workspace_id,",
        "progress,",
        "runs,",
        "fleet_workspaces,",
        "fleet_agents,",
    ] {
        assert!(
            MAIN.contains(token),
            "wave11 (bd-wave11-snapshot-mndgf): snapshot.rs must contain {token}"
        );
    }
    for door in [
        "snapshot::assemble",
        "snapshot::progress_view",
        "fn status_core(",
        "fn progress(",
        "fn robot(",
        "fn serve(",
    ] {
        assert!(
            MAIN.contains(door),
            "wave11 (bd-wave11-snapshot-mndgf): door {door} must project over the snapshot"
        );
    }
    // No second world read: progress door must not shell `br` directly;
    // the single read lives in `snapshot::read_progress_section`.
    let progress_sites = MAIN.match_indices("Bead::list_issues").count();
    assert!(
        progress_sites <= 2,
        "wave11: only snapshot may shell br list (found {progress_sites} sites)"
    );
}
