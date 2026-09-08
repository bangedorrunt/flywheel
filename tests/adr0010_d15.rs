//! ADR-0010 D15: ask-user-authority; worker never self-answers.
//! Policy text in templates (peer-aware + ce-execute + ce-review bodies);
//! closes through D6 (OPEN DECISIONS) and the existing tick LoudMiss.
//! Test names: adr0010_d15_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d15`.

const TPL: &str = include_str!("../integrations/peer-aware.md.tpl");
const STAGES: &str = include_str!("../src/stages.rs");
const WORKSPACE: &str = include_str!("../src/workspace.rs");

#[test]
fn adr0010_d15_worker_never_self_answers() {
    for needle in ["ask-user", "never answer", "self-answer"] {
        assert!(TPL.contains(needle), "peer-aware template missing {needle}");
    }
    // Both stage bodies carry the policy (2 occurrences in stages.rs).
    let hits = STAGES.matches("ask-user").count();
    assert!(
        hits >= 2,
        "ce-execute + ce-review bodies must carry ask-user policy, found {hits}"
    );
}

#[test]
fn adr0010_d15_restore_vs_expand_routing() {
    // Restorations auto-decide; expansions escalate with five elements and
    // park in OPEN DECISIONS via Publish corr + WaitCorr.
    for needle in [
        "restoration",
        "expansion",
        "smallest",
        "consequences",
        "recommendation",
        "WaitCorr",
        "OPEN DECISIONS",
    ] {
        assert!(
            TPL.contains(needle) || STAGES.contains(needle),
            "policy text missing {needle}"
        );
    }
}

#[test]
fn adr0010_d15_self_close_stays_loudmiss() {
    // Worker-side auto-close of an ask-user finding is a LoudMiss —
    // the existing tick rule (unknown corr) that D15 closes through.
    assert!(
        WORKSPACE.contains("HumanDecision for unknown corr"),
        "tick must LoudMiss answers to unknown corrs"
    );
}
