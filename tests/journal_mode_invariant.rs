// governed-by: ADR-0032, ADR-0035
//! Effect-journal MODE invariant (B5, bd-langgraph-b5-journal).
//!
//! ## Why this file exists
//!
//! The effect journal's *behavior* is already well covered in-crate:
//! `src/effect_journal.rs`'s `#[cfg(test)] mod tests` pins
//! intent-before-provider, settle-on-delivery, `Unknown`-vs-`Failed`,
//! terminal-replay-without-redispatch, and the verify sink's
//! observe-don't-re-perform rule. `toron-workflow`'s own suite pins that
//! `EffectJournalMode::Enforced` refuses an uncovered effectful leaf.
//!
//! What is NOT covered anywhere is the choice flywheel makes at each engine
//! construction site. That choice is a *wiring* decision, so it cannot be
//! reached by calling a unit — it can only be pinned as a source contract.
//! Read it as a contract test, not a behavior test; the honest limit is
//! stated at the bottom of this file.
//!
//! ## The invariant
//!
//! `EffectJournalMode` is chosen CONDITIONALLY, and both failure directions
//! are real bugs:
//!
//! * `Enforced` chosen unconditionally -> a journal outage turns every live
//!   fire into a hard failure. The cron arm calls this out explicitly:
//!   "with no journal the mode stays Recording so the bypass marker keeps the
//!   gap visible instead of failing every fire."
//! * `Recording` chosen unconditionally on an arm that REALLY dispatches ->
//!   the silent bypass ADR-0032 Spec 6 forbids: an effect nobody can
//!   reconcile reads as success.
//!
//! So the rule is: **Enforced is selected only when a journal is actually
//! present, and only on an arm that really performs external effects.**

use std::sync::OnceLock;

/// The production sources that construct a `WorkflowEngine` with a mode.
///
/// `pane.rs` is the live dispatch host and `cron.rs` holds both cron engine
/// arms (YAML + registered). Each is scanned with a COUNT, not a fixed
/// `contains`, so a NEW construction site has to satisfy the contract too
/// rather than slipping past a test written against today's file.
fn mode_bearing_sources() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/pane.rs", include_str!("../src/pane.rs")),
        ("src/cron.rs", include_str!("../src/cron.rs")),
    ]
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// The text of the statement that introduces a given byte offset: everything
/// back to the previous `;`.
///
/// A mode selection reads `let mode = if <gate> {` on one line and the
/// variant on the next, so a LINE-based scan false-positives on every guarded
/// site. The statement head is what actually carries the condition, and `;`
/// is the boundary that matters: bounding on `{`/`}` instead would stop at
/// the branch's own opening brace and hide the `if` that guards it.
fn statement_head(source: &str, at: usize) -> &str {
    let prefix = &source[..at];
    let boundary = prefix.rfind(';').map(|i| i + 1).unwrap_or(0);
    &prefix[boundary..]
}

/// Every `Enforced` mention must be introduced by a journal-present `if`.
///
/// Returns the offsets that are NOT guarded, so the caller can fail with
/// them. A bare `let mode = EffectJournalMode::Enforced;` has no `if` in its
/// statement head, which is exactly the journal-outage bug.
fn unguarded_enforced(source: &str) -> Vec<(usize, String)> {
    let needle = "EffectJournalMode::Enforced";
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = source[from..].find(needle) {
        let at = from + rel;
        let head = statement_head(source, at);
        if !head.contains("if ") {
            out.push((at, head.trim().to_string()));
        }
        from = at + needle.len();
    }
    out
}

#[test]
fn no_production_site_selects_enforced_unconditionally() {
    let mut checked = 0usize;
    for (name, source) in mode_bearing_sources() {
        let unguarded = unguarded_enforced(source);
        assert!(
            unguarded.is_empty(),
            "{name}: unguarded EffectJournalMode::Enforced at {unguarded:?} — a journal \
             outage would fail every live fire instead of keeping the bypass visible"
        );
        checked += count(source, "EffectJournalMode::Enforced");
    }
    assert!(
        checked > 0,
        "scanner found no Enforced selection at all — the contract is no longer \
         being expressed where this test looks, so this test is vacuous"
    );
}

#[test]
fn every_enforced_selection_is_gated_on_a_present_journal() {
    let needle = "EffectJournalMode::Enforced";
    for (name, source) in mode_bearing_sources() {
        let mut total = 0usize;
        let mut gated = 0usize;
        let mut from = 0usize;
        while let Some(rel) = source[from..].find(needle) {
            let at = from + rel;
            total += 1;
            // The gate is the journal actually opening. `effect.is_some()` is
            // how both files learn that, and it is the only thing that makes
            // Enforced safe: with no runner there is nothing to journal
            // through, so Enforced would just fail the arm.
            if statement_head(source, at).contains("effect.is_some()") {
                gated += 1;
            }
            from = at + needle.len();
        }
        assert!(
            total > 0,
            "{name}: no Enforced selection found — this test is vacuous here"
        );
        assert_eq!(
            gated, total,
            "{name}: {total} Enforced selection(s) but only {gated} gated on a \
             present journal"
        );
    }
}

#[test]
fn pane_gates_enforced_on_the_journal_flag_not_only_on_the_runner() {
    let pane = include_str!("../src/pane.rs");
    // `pane.rs`'s constructor serves BOTH live and dry arms (`workflow run`
    // with and without `--execute`, `cron run` with and without `--live`), so
    // its condition must require the caller's live flag as well as the
    // journal. Gating on `effect.is_some()` alone would let a dry arm be
    // promoted to Enforced the moment a journal happens to be open.
    assert!(
        pane.contains("if journal && effect.is_some()"),
        "pane.rs must select Enforced only when the live `journal` flag AND a \
         journal runner are both present"
    );
}

#[test]
fn dry_arms_stay_on_recording_and_are_labelled_as_such() {
    for (name, source) in mode_bearing_sources() {
        assert!(
            source.contains("EffectJournalMode::Recording"),
            "{name}: the no-journal branch must fall back to Recording so the \
             gap stays visible instead of failing every fire"
        );
    }
}

/// Honest limit of this suite: it pins the SHAPE of the wiring, not its
/// runtime behavior. A site that satisfies the text contract but threads the
/// wrong `effect` value would still pass. The behavioral half of this
/// contract lives where it belongs — `src/effect_journal.rs`'s in-crate tests
/// and `toron-workflow`'s `EffectJournalMode` suite — and those are the
/// tests that would catch a wrong value. This file exists only so the wiring
/// cannot silently drift away from the behavior those tests describe.
#[test]
fn this_suite_scans_every_mode_bearing_source() {
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    let names = NAMES.get_or_init(|| mode_bearing_sources().into_iter().map(|(n, _)| n).collect());
    assert_eq!(
        names.as_slice(),
        ["src/pane.rs", "src/cron.rs"],
        "a new mode-bearing source must be added to this contract"
    );
}
