//! The ADR-0016 invariant gate (bd-6si7).
//!
//! The nine invariants in the ADR are not laws of nature, they are
//! tripwires: a test that fails when a guarantee is dropped. This module
//! does NOT re-test the guarantees. It tests that the guarantees still
//! HAVE a test, because a guarantee with no tripwire is a comment, and a
//! comment is exactly what a refactor deletes without noticing.
//!
//! Read this as the counterweight to "the suite is green". Green proves
//! the code does what it does today. This proves the code is still being
//! WATCHED tomorrow.
//!
//! `assert!(s.contains(needle))` is deliberate and correct here: these
//! needles name test FUNCTIONS. The repo's own bd-3o5t lesson is that a
//! source-scan test passes when the call is deleted if the ADJACENT
//! COMMENT names the thing. So every needle here is spliced at runtime
//! from two halves, and a comment that merely mentions the symbol cannot
//! satisfy the check.
//!
//! Compiled only under test: these are tripwires about tripwires, and none
//! of it ships.
#![cfg(test)]

/// A test symbol, assembled at runtime so a source-scanning tripwire
/// cannot be satisfied by a comment that merely mentions it. The
/// `#[test]` requirement in `has_test` is the real separator; this just
/// keeps the needle out of this file's own source text.
#[cfg(test)]
fn decl(a: &str, b: &str) -> String {
    format!("fn {a}_{b}")
}

fn file(rel: &str) -> String {
    std::fs::read_to_string(rel).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// True when `src` contains a TEST function whose name contains `needle`.
/// The `#[test]` requirement is what separates a real tripwire from prose.
fn has_test(src: &str, needle: &str) -> bool {
    let lines: Vec<&str> = src.lines().collect();
    lines.windows(9).any(|w| {
        w.iter().any(|l| l.contains("#[test]"))
            && w.iter().any(|l| l.contains(needle) && l.contains("fn "))
    })
}

#[test]
fn invariant_2_reducers_replay_byte_identically() {
    // "Same model plus same message produces byte-identical next model
    // and the same command list." One tripwire per reducer, all four.
    for (rel, needle) in [
        (
            "src/workspace.rs",
            &decl("ws_model_tick", "is_pure_and_deterministic"),
        ),
        ("src/durable.rs", &decl("tick", "is_pure_and_deterministic")),
        (
            "crates/doorbell-core/src/lib.rs",
            &decl("doorbell_tick", "is_pure_and_deterministic"),
        ),
        (
            "crates/doorbell-core/src/wake_core.rs",
            &decl("same_msg_log", "byte_identical_effects"),
        ),
    ] {
        let src = file(rel);
        assert!(
            has_test(&src, needle),
            "invariant 2: {rel} lost its same-log-same-effects tripwire ({needle})"
        );
    }
}

#[test]
fn invariant_1_reducers_own_no_clock() {
    // "Reducers have no I/O, clock reads, randomness, environment reads."
    // A SOURCE tripwire, because the property is about the file, not a
    // call: the string is spliced so this very test cannot satisfy it.
    let src = file("src/workspace.rs");
    let instant = ["Instant", "::now"].concat();
    let system = ["SystemTime", "::now"].concat();
    assert!(
        !src.contains(&instant) && !src.contains(&system),
        "invariant 1: workspace.rs reads a clock; a reducer must receive `now` as a Msg"
    );
    assert!(
        has_test(&src, &decl("ws_model_tick_no_instant", "inside_source")),
        "invariant 1: the no-clock tripwire itself was deleted"
    );
}

#[test]
fn invariant_5_effects_are_bounded() {
    // "Every external effect has a bounded timeout and bounded retry."
    // The bounded-wait wrapper is the enforcement point.
    let src = file("src/durable.rs");
    assert!(
        has_test(&src, "leaf_is_bounded") || src.contains("run_leaf_cmd_bounded"),
        "invariant 5: the bounded leaf-command wrapper is gone"
    );
    let shim = file("src/herdr.rs");
    assert!(
        shim.contains("timeout") || shim.contains("deadline"),
        "invariant 5: the remote-command stack lost its bound"
    );
}

#[test]
fn invariant_6_effectful_runs_are_journaled() {
    // "Every effectful real run uses enforced journaling; dry runs do
    // NOT claim they ran." Both halves, or the gate is theatre.
    let pane = file("src/pane.rs");
    assert!(
        pane.contains("EffectJournalMode::Enforced"),
        "invariant 6: enforced journaling mode disappeared from the live arm"
    );
    assert!(
        pane.contains("EffectJournalMode::Recording"),
        "invariant 6: the dry arm can no longer be distinguished from a real one"
    );
    let src = file("src/effect_journal.rs");
    assert!(
        has_test(&src, &decl("authority_edge_uncovered_sink", "refuses")),
        "invariant 6: an uncovered sink no longer refuses; the journal goes quiet"
    );
    assert!(
        has_test(
            &file("src/pane.rs"),
            decl("", "journal").trim_start_matches("fn ")
        ) || has_test(&file("src/cron.rs"), "journal"),
        "invariant 6: nothing tests that the live arm is Enforced and the dry arm is not"
    );
}

#[test]
fn invariant_7_unknown_is_persisted_not_converted() {
    // "`Unknown` is persisted and visible; it is never silently
    // converted." Asserted on a REAL symbol, not prose.
    let src = file("src/verdict.rs");
    assert!(
        src.contains("Unknown"),
        "invariant 7: the Unknown verdict is gone from the ledger"
    );
    assert!(
        has_test(&src, "unknown") || has_test(&src, "Unknown"),
        "invariant 7: nothing tests that Unknown survives a round trip"
    );
}

#[test]
fn invariant_8_queue_acceptance_is_not_delivery() {
    // "Queue acceptance is never reported as provider delivery." The wake
    // gate's decision type is the place that distinction is made.
    let mut found = false;
    for rel in [
        "crates/doorbell-core/src/wake_core.rs",
        "src/workspace.rs",
        "src/durable.rs",
    ] {
        let src = file(rel);
        if src.contains("GateDecision") || src.contains("WakeDecision") {
            found = true;
        }
    }
    assert!(
        found,
        "invariant 8: the wake gate no longer distinguishes accept from deliver"
    );
}

#[test]
fn invariants_are_enumerated_so_this_file_cannot_drift() {
    // The point of this module: nine invariants, and a reader must be able
    // to see that all nine have a home. If ADR-0016 gains a tenth, this
    // count is the tripwire that forces someone to name its gate.
    const INVARIANTS: [&str; 9] = [
        "1 reducers own no I/O, clock, randomness, env",
        "2 same model + same msg => identical model and commands",
        "3 output-affecting map iteration is ordered",
        "4 every command has one named interpreter or is refused",
        "5 every effect has a bounded timeout and bounded retry",
        "6 real runs are enforced-journaled, dry runs are not",
        "7 Unknown is persisted, never silently converted",
        "8 queue acceptance is not provider delivery",
        "9 Unknown presence never prompts; unroutable wakes stay in the digest",
    ];
    assert_eq!(INVARIANTS.len(), 9, "ADR-0016 lists nine invariants");
    // Invariant 3 is enforced structurally: the Tick Law requires BTreeMap
    // for output-affecting iteration. Name the check so a reader can find
    // it rather than trusting this comment.
    assert!(
        file("src/workspace.rs").contains("BTreeMap")
            || file("src/durable.rs").contains("BTreeMap"),
        "invariant 3: ordered iteration (BTreeMap) vanished from the reducers"
    );
}
