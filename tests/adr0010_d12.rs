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
/// bd-jhz8x.2: GATE is an engine step (`__wave-gate`), so the bounded
/// projection's *caller* is this file, not the retired wave-gate prompt.
const WAVE_GATE: &str = include_str!("../src/cmd/wave_gate.rs");
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
    assert!(STAGES.contains('7'), "perishable TTL 7d in stages.rs");
    assert!(
        STAGES.contains("4000") || STAGES.contains("4_000") || MAIN.contains("4000"),
        "default GATE budget ~4k tokens"
    );
}

/// The property ADR-0010 D12 pins is "the gate's grounding read IS the bounded
/// projection, under the budget knob" — not "a prompt says so". The gate is an
/// engine step now (`__wave-gate`), so the assertion follows the caller: the
/// step runs the shared projection and carries its paths into the artifact
/// DISPATCH reads. The prompt that used to name the command is retired.
#[test]
fn adr0010_d12_gate_stage_uses_the_bounded_projection() {
    assert!(
        WAVE_GATE.contains("project_gate_learnings"),
        "the gate step runs the bounded projection (not a bare corpus scan)"
    );
    assert!(
        WAVE_GATE.contains("default_gate_budget"),
        "the gate step reads the budget knob (FLYWHEEL_GATE_BUDGET)"
    );
    assert!(
        WAVE_GATE.contains("\"learnings\"") && WAVE_GATE.contains("artifact[\"learnings\"]"),
        "the projected paths ride the artifact, which is what DISPATCH reads"
    );
    // The projection itself (ranking, TTLs, budget) still lives in stages.rs.
    assert!(
        STAGES.contains("fn gate_projection") && STAGES.contains("FLYWHEEL_GATE_BUDGET"),
        "the bounded projection layer is unchanged"
    );
}

#[test]
fn adr0010_d12_ce_sweep_backfills_and_archives() {
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
fn bd_21og_ce_compound_depth_lightweight_or_full_default_full() {
    for needle in ["- name: depth", "lightweight", "{{inputs.depth}}"] {
        assert!(
            WORKFLOWS.contains(needle),
            "ce-compound depth missing {needle} (builtin_workflows.rs)"
        );
    }
    assert!(
        WORKFLOWS.contains("default: \"full\""),
        "ce-compound depth must default full"
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
