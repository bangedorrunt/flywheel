//! ADR-0010 D6: OPEN DECISIONS fold, captain-hold cache, pending_wait promotes.
//! Binary-only crate: structural assertions scan source (D0/D3/D4 pattern);
//! behavioral tests live in src/workspace.rs unit tests.
//! Test names: adr0010_d6_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d6`.

const WORKSPACE: &str = include_str!("../src/workspace.rs");

#[test]
fn adr0010_d6_open_decisions_fold_present() {
    for needle in [
        "fn open_decisions",
        "fn render_open_decisions",
        "OPEN DECISIONS",
    ] {
        assert!(WORKSPACE.contains(needle), "workspace.rs missing {needle}");
    }
}

#[test]
fn adr0010_d6_divergence_never_closes() {
    for needle in [
        "fn record_divergence",
        "ClosedBeadOpenCorr",
        "AnsweredCorrHeldBead",
        "DoneWithoutLegalClose",
    ] {
        assert!(WORKSPACE.contains(needle), "workspace.rs missing {needle}");
    }
}

#[test]
fn adr0010_d6_pending_wait_promotes_and_only_answer_clears() {
    // Expiry re-surfaces into open_human (promote, never drop) alongside
    // the existing wait-timeout audit.
    assert!(
        WORKSPACE.contains("pending-wait expired") || WORKSPACE.contains("re-surface"),
        "sweep expiry must promote into open_human"
    );
    // The only remover of open_human entries is the answer path.
    let removers = WORKSPACE.matches("open_human.remove").count();
    assert_eq!(
        removers, 1,
        "exactly one open_human.remove site (answer path), found {removers}"
    );
}
