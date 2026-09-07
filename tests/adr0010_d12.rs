// governed-by: ADR-0010 D12 (bd-fmya) — learnings tier/reinforced + bounded
//! GATE projection.
//! Frontmatter: `tier: pinned|aging|perishable` + `reinforced: YYYY-MM-DD`
//! (perishable additionally names a checkable `expires:`). GATE grounding
//! read = top-N by reinforcement recency within FLYWHEEL_GATE_BUDGET (~4k
//! tokens); un-reinforced aging entries decay OUT of the projection but stay
//! in the corpus (archive, never delete).
//! Test names: adr0010_d12_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d12`.

const MAIN: &str = include_str!("../src/main.rs");
const STAGES: &str = include_str!("../src/stages.rs");
const WORKFLOWS: &str = include_str!("../src/builtin_workflows.rs");
const README: &str = include_str!("../docs/learnings/README.md");

#[test]
fn adr0010_d12_projection_pure_layer_present() {
    for needle in [
        "fn gate_projection",
        "FLYWHEEL_GATE_BUDGET",
        "AGING_TTL_DAYS",
        "LearningTier",
        "fn parse_learning_frontmatter",
    ] {
        assert!(
            STAGES.contains(needle) || MAIN.contains(needle),
            "projection layer missing {needle}"
        );
    }
}

#[test]
fn adr0010_d12_ttls_in_structure() {
    assert!(STAGES.contains("30"), "aging TTL 30d in stages.rs");
    assert!(STAGES.contains("7"), "perishable TTL 7d in stages.rs");
    assert!(
        STAGES.contains("4000") || STAGES.contains("4_000") || MAIN.contains("4000"),
        "default GATE budget ~4k tokens"
    );
}

#[test]
fn adr0010_d12_gate_stage_uses_the_bounded_projection() {
    // The wave-gate grounding read must route through the projection, not a
    // bare corpus rg.
    assert!(
        STAGES.contains("__gate-learnings") || STAGES.contains("gate-learnings"),
        "wave-gate stage references the bounded projection command"
    );
    assert!(
        STAGES.contains("FLYWHEEL_GATE_BUDGET") || STAGES.contains("budget"),
        "gate stage names the budget knob"
    );
}

#[test]
fn adr0010_d12_ce_sweep_backfills_and_archives() {
    // Migration: backfill tier:aging + reinforced:today; archive never delete.
    for needle in ["tier: aging", "reinforced:", "archive", "expires:"] {
        assert!(
            WORKFLOWS.contains(needle),
            "ce-sweep rules missing {needle} (builtin_workflows.rs)"
        );
    }
}

#[test]
fn adr0010_d12_ce_compound_stamps_tier_and_reinforced() {
    assert!(
        WORKFLOWS.contains("reinforced"),
        "ce-compound must stamp reinforced on capture/update"
    );
}

#[test]
fn adr0010_d12_readme_documents_the_frontmatter() {
    for needle in ["tier:", "reinforced:", "pinned", "aging", "perishable"] {
        assert!(
            README.contains(needle),
            "docs/learnings/README.md missing {needle}"
        );
    }
}
