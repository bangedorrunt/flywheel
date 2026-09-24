// governed-by: bd-iqte — Chiebukuro compound wiring with auto-promote inbox.
//! ce-compound read lanes (rg+qmd+recall, labeled, offline-safe) + memory
//! record on write + auto-promote to docs/learnings/inbox/ as
//! tier-perishable with provenance; ce-sweep reinforces or archives. Human
//! gate moves write-time to sweep-time per operator decision.
//! Test names: ce_compound_* — filtered by `mbx test ce_compound`.

const STAGES: &str = include_str!("../src/stages.rs");
const WORKFLOWS: &str = include_str!("../src/builtin_workflows.rs");
const INBOX_README: &str = include_str!("../docs/learnings/inbox/README.md");

#[test]
fn ce_compound_read_lanes_labeled_offline_safe() {
    // Three labeled read lanes, no web: corpus rg, wiki qmd, episodic recall.
    for needle in [
        "rg -il",
        "chie qmd search",
        "chie memory recall",
        "corpus",
        "wiki",
        "memory",
    ] {
        assert!(
            STAGES.contains(needle),
            "compound read lane missing {needle:?} (src/stages.rs)"
        );
    }
}

#[test]
fn ce_compound_memory_record_on_write() {
    assert!(
        STAGES.contains("chie memory record"),
        "compound must memory-record on write (src/stages.rs)"
    );
}

#[test]
fn ce_compound_auto_promote_inbox_perishable_with_provenance() {
    // Auto-promote target is the inbox, tier-perishable with provenance —
    // never straight to docs/learnings/, never gated on a human at write.
    for needle in [
        "docs/learnings/inbox/",
        "tier: perishable",
        "expires:",
        "provenance",
    ] {
        assert!(
            STAGES.contains(needle) || WORKFLOWS.contains(needle),
            "auto-promote inbox missing {needle:?}"
        );
    }
    assert!(
        INBOX_README.contains("perishable") && INBOX_README.contains("provenance"),
        "inbox README must state the perishable+provenance contract"
    );
}

#[test]
fn ce_compound_human_gate_at_sweep_not_write() {
    // Write-time is filing, not approval: the human reviews at sweep.
    assert!(
        STAGES.contains("sweep"),
        "compound must defer human review to sweep (src/stages.rs)"
    );
    for needle in ["ce-sweep", "inbox"] {
        assert!(
            WORKFLOWS.contains(needle),
            "sweep must triage the inbox ({needle:?} in builtin_workflows.rs)"
        );
    }
}
