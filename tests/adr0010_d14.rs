//! ADR-0010 D14: captain-facing turn-end guard (300s beacon).
//! Binary-only crate: structural assertions scan source (D0/D3/D4 pattern);
//! behavioral tests live in src/main.rs unit tests.
//! Test names: adr0010_d14_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d14`.

const MAIN: &str = include_str!("../src/main.rs");

#[test]
fn adr0010_d14_guard_predicate_present() {
    for needle in [
        "fn turn_end_guard",
        "FLYWHEEL_GUARD_GRACE_SECS",
        "TurnDecision",
        "follow_up",
    ] {
        assert!(MAIN.contains(needle), "main.rs missing {needle}");
    }
    assert!(MAIN.contains("300"), "guard grace default 300s must appear");
}

#[test]
fn adr0010_d14_crew_exempt_afk_satisfies() {
    // Crew panes are exempt; a fresh identity-matched AFK beacon satisfies.
    assert!(
        MAIN.contains("is_crew"),
        "predicate must take a crew-exemption input"
    );
    assert!(
        MAIN.contains("afk_satisfied") || MAIN.contains("afk_live"),
        "predicate must take an AFK-satisfaction input (D5 daemon)"
    );
}

#[test]
fn adr0010_d14_hook_command_wired() {
    // Hidden hook plumbing (like __dispatch-guard): vendor Stop hooks call
    // it; nonzero exit blocks the turn-end.
    assert!(
        MAIN.contains("turn-guard") || MAIN.contains("turn_guard"),
        "turn-guard hook command must be wired"
    );
}
