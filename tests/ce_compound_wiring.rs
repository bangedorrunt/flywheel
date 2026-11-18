// governed-by: bd-t5f0v — continual learning on every path; chiebukuro is the ONLY store.
//! ce-compound read lanes (chie qmd + recall + check-trap, labeled, offline-safe)
//! + `record-lesson` on write; no repo-side corpus, inbox, or perishable file.
//!
//! Test names: ce_compound_* — filtered by `mbx test ce_compound`.

const STAGES: &str = include_str!("../src/stages.rs");
const WORKFLOWS: &str = include_str!("../src/builtin_workflows.rs");

#[test]
fn ce_compound_read_lanes_labeled_offline_safe() {
    for needle in [
        "chie qmd search",
        "chie memory recall",
        "flywheel memory check-trap",
        "wiki",
        "memory",
        "traps",
    ] {
        assert!(
            STAGES.contains(needle),
            "compound read lane missing {needle:?} (src/stages.rs)"
        );
    }
}

#[test]
fn ce_compound_record_lesson_on_write() {
    assert!(
        STAGES.contains("record-lesson"),
        "compound must write with record-lesson (src/stages.rs)"
    );
    assert!(
        WORKFLOWS.contains("record-lesson"),
        "the standalone ce-compound workflow must write with record-lesson"
    );
}

#[test]
fn ce_compound_has_no_repo_side_corpus_or_inbox() {
    let retired_path = ["docs/", "learnings"].concat();
    for text in [STAGES, WORKFLOWS] {
        for needle in [retired_path.as_str(), "inbox/", "tier: perishable"] {
            assert!(
                !text.contains(needle),
                "the learning store is chiebukuro only; found {needle:?}"
            );
        }
    }
}

#[test]
fn ce_compound_sweep_maintenance_is_chie_promote_forget() {
    assert!(
        STAGES.contains("chie memory promote") && STAGES.contains("chie memory forget"),
        "sweep maintenance must be chie promote/forget (src/stages.rs)"
    );
    assert!(
        WORKFLOWS.contains("chie memory promote") && WORKFLOWS.contains("chie memory forget"),
        "the ce-sweep workflow must maintain chie records"
    );
}
