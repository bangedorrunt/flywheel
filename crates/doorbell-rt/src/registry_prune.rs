//! Registry pruning on POSITIVE EVIDENCE OF DEATH. bd-3uqz.
//!
//! ## Why this is a separate module
//!
//! The reaper it replaces lives inside `FileHost` and reads the clock, the audit
//! log, and two mutable maps. That coupling is why the rule it got wrong was hard
//! to see: the decision itself is a pure function of (rows, what herdr answered,
//! how many times we have seen this), and nothing about it needs a host.
//!
//! So the decision lives here, pure, and `FileHost` keeps only the plumbing. The
//! two fences below are the reason: they are about the DECISION, and they can be
//! written and falsified without a daemon, a clock, or a temp state dir.
//!
//! ## The rule, and the number it is protecting
//!
//! **Prune only on positive evidence that a pane died. Never on absence of
//! evidence that it lives.**
//!
//! Live on 2026-09-28 the injection picture read `pins: 7, panes_seen: 2,
//! panes_missing: 13` — so **13 of 20 rows (65%)** had a pane herdr did not list. A
//! pruner keyed on "pane not in herdr" would delete two thirds of the routing table
//! on its first sweep, and would do it precisely when herdr is unhealthy, which is
//! exactly when the table matters most.
//!
//! Three categories, three rules:
//!
//! 1. **Pane listed by herdr** — alive. Confirmation count resets to zero.
//! 2. **Row has an EMPTY `pane`** — never pruned. A pane-less row resolves by
//!    LABEL, which is the legacy shape that still works; `prompt_hop_keeps_the_
//!    label_when_the_row_has_no_pane` exists for exactly this. The reaper being
//!    replaced did prune these, because its liveness test was
//!    `!pane.is_empty() && known.contains(pane)` — so an empty pane never matched
//!    "alive" and fell straight through to the reap branch. No test covered it.
//! 3. **Pane non-empty and NOT listed** — accumulate one CONFIRMATION. Prune only
//!    after `required` consecutive observations.
//!
//! And the outer guard: **if herdr answered with nothing at all, there is no
//! evidence, so nothing is pruned and nothing is even counted.** A herdr restart, a
//! transient API failure and a genuinely empty session are indistinguishable from
//! the outside, and all three must be treated as "do not know".
//!
//! ## Why confirmations and not a clock
//!
//! The reaper being replaced used `STALE_PANE_GRACE_SECS` — a wall-clock window.
//! A time window is a PROXY for "we have seen this repeatedly", and it is wrong in
//! both directions: one herdr outage that outlasts the window prunes a live fleet,
//! while a pane that dies and returns within the window is never pruned. Counting
//! consecutive observations measures the thing itself. The clock still belongs to
//! the host; this function takes the count as an input.
//!
//! ## Tombstones, because a delete has no memory
//!
//! A pruned row leaves a [`Tombstone`] naming the pin, the pane, the number of
//! confirmations, and why. A pane that comes back can be re-registered against it,
//! and weeks later "why does this pin not wake?" is still answerable. A silent
//! delete makes the registry's history unreconstructable, which is how you end up
//! debugging a routing table with no memory of what it used to contain.

/// How many consecutive observations of a vanished pane are required before a row
/// is pruned. Three is enough to ride out a herdr restart without being slow to
/// notice a pane that is genuinely gone.
pub const REQUIRED_CONFIRMATIONS: u32 = 3;

/// One registry row, reduced to what the decision needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub pin: String,
    /// Empty means "no pane recorded" — the legacy label-routed shape.
    pub pane: String,
}

/// What a prune decided to keep, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// herdr listed the pane on this observation.
    Alive,
    /// The row has no pane recorded, so it resolves by label. Never pruned.
    LabelRouted,
    /// Pane not listed, but not yet enough times to call it dead.
    Unconfirmed,
    /// Pane not listed `required` times running. Positive evidence of death.
    Pruned,
    /// herdr answered nothing, so nothing was learned. Kept AND uncounted.
    NoEvidence,
}

/// A record that a row was pruned, kept so the decision is reconstructable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub pin: String,
    pub pane: String,
    pub confirmations: u32,
    pub why: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub fates: Vec<(String, Fate)>,
    pub tombstones: Vec<Tombstone>,
}

impl Decision {
    pub fn fate_of(&self, pin: &str) -> Option<Fate> {
        self.fates.iter().find(|(p, _)| p == pin).map(|(_, f)| *f)
    }
    pub fn pruned(&self) -> Vec<&str> {
        self.fates
            .iter()
            .filter(|(_, f)| *f == Fate::Pruned)
            .map(|(p, _)| p.as_str())
            .collect()
    }
}

/// The one rule: a vanished pane is confirmed dead only by REPEATED observation
/// while herdr is demonstrably answering.
///
/// `known_panes` empty means herdr did not answer. That is not "every pane is
/// gone"; it is "we do not know", and the two must never be conflated — which is
/// the entire 65% hazard in one branch.
pub fn decide(
    rows: &[Row],
    known_panes: &std::collections::BTreeSet<String>,
    confirmations: &mut std::collections::BTreeMap<String, u32>,
    required: u32,
) -> Decision {
    let mut fates = Vec::with_capacity(rows.len());
    let mut tombstones = Vec::new();

    if known_panes.is_empty() {
        // No evidence. Keep every row and do NOT touch the counters, so an outage
        // cannot manufacture confirmations for panes that are perfectly alive.
        return Decision {
            fates: rows
                .iter()
                .map(|r| (r.pin.clone(), Fate::NoEvidence))
                .collect(),
            tombstones,
        };
    }

    for row in rows {
        let fate = if row.pane.is_empty() {
            // Rule 2. The reaper being replaced pruned these.
            confirmations.remove(&row.pin);
            Fate::LabelRouted
        } else if known_panes.contains(&row.pane) {
            // Rule 1.
            confirmations.remove(&row.pin);
            Fate::Alive
        } else {
            // Rule 3.
            let seen = confirmations.entry(row.pin.clone()).or_insert(0);
            *seen = seen.saturating_add(1);
            if *seen >= required {
                tombstones.push(Tombstone {
                    pin: row.pin.clone(),
                    pane: row.pane.clone(),
                    confirmations: *seen,
                    why: "pane absent from herdr across consecutive confirmations",
                });
                confirmations.remove(&row.pin);
                Fate::Pruned
            } else {
                Fate::Unconfirmed
            }
        };
        fates.push((row.pin.clone(), fate));
    }
    Decision { fates, tombstones }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn rows() -> Vec<Row> {
        vec![
            Row {
                pin: "LivePin".into(),
                pane: "w9:p1".into(),
            },
            Row {
                pin: "DoomedPin".into(),
                pane: "w11:p1".into(),
            },
            Row {
                pin: "LabelPin".into(),
                pane: String::new(),
            },
        ]
    }

    fn herdr(panes: &[&str]) -> std::collections::BTreeSet<String> {
        panes.iter().map(|s| (*s).to_string()).collect()
    }

    /// THE FENCE THAT STOPS THE 65% WIPE, and the one the user insisted must exist
    /// BEFORE the pruning code. A herdr restart answers with an empty pane list
    /// three times running; not one row may be pruned, and not one confirmation
    /// counter may move. Without the second half this test also passes for a
    /// pruner that prunes nothing ever, which is the failure I have been closing all
    /// session in the other direction.
    #[test]
    fn a_herdr_restart_prunes_nothing_and_counts_nothing() {
        let mut c: BTreeMap<String, u32> = BTreeMap::new();
        for sweep in 1..=5 {
            let d = decide(&rows(), &Default::default(), &mut c, REQUIRED_CONFIRMATIONS);
            assert_eq!(
                d.pruned(),
                Vec::<&str>::new(),
                "sweep {sweep}: an empty pane list is NO EVIDENCE and must prune nothing"
            );
            assert_eq!(
                d.fates,
                rows()
                    .iter()
                    .map(|r| (r.pin.clone(), Fate::NoEvidence))
                    .collect::<Vec<_>>(),
                "sweep {sweep}: every row must be NoEvidence"
            );
            assert!(
                d.tombstones.is_empty(),
                "sweep {sweep}: no tombstones either"
            );
        }
        assert!(
            c.is_empty(),
            "an outage must not manufacture confirmations, got {c:?}"
        );
    }

    /// THE OTHER FENCE, and the one that stops "prune on positive evidence" from
    /// being unfalsifiable. Absent the previous test, a pruner that NEVER prunes
    /// would satisfy every other assertion in this file.
    ///
    /// A pane that was listed and is now gone, seen three times running, IS dead.
    #[test]
    fn three_consecutive_confirmations_of_a_vanished_pane_prune() {
        let mut c: BTreeMap<String, u32> = BTreeMap::new();
        let known = herdr(&["w9:p1"]);

        let d1 = decide(&rows(), &known, &mut c, REQUIRED_CONFIRMATIONS);
        assert_eq!(
            d1.fate_of("DoomedPin"),
            Some(Fate::Unconfirmed),
            "first sighting"
        );
        assert_eq!(d1.pruned(), Vec::<&str>::new());

        let d2 = decide(&rows(), &known, &mut c, REQUIRED_CONFIRMATIONS);
        assert_eq!(d2.fate_of("DoomedPin"), Some(Fate::Unconfirmed), "second");

        let d3 = decide(&rows(), &known, &mut c, REQUIRED_CONFIRMATIONS);
        assert_eq!(
            d3.fate_of("DoomedPin"),
            Some(Fate::Pruned),
            "the third consecutive sighting is positive evidence of death"
        );
        assert_eq!(d3.pruned(), vec!["DoomedPin"]);
        assert_eq!(
            d3.fate_of("LivePin"),
            Some(Fate::Alive),
            "a listed pane is alive"
        );
    }

    /// The live bug, pinned. The reaper being replaced used
    /// `!pane.is_empty() && known.contains(pane)` as its liveness test, so a row
    /// with an EMPTY pane never matched "alive", fell through to the reap branch,
    /// and was pruned once the grace window elapsed — while
    /// `prompt_hop_keeps_the_label_when_the_row_has_no_pane` documents that exactly
    /// those rows still resolve by label. No test covered the intersection.
    #[test]
    fn a_row_with_no_pane_is_never_pruned_however_long_it_is_unlisted() {
        let mut c: BTreeMap<String, u32> = BTreeMap::new();
        for _ in 0..50 {
            let d = decide(&rows(), &herdr(&["w9:p1"]), &mut c, REQUIRED_CONFIRMATIONS);
            assert_eq!(
                d.fate_of("LabelPin"),
                Some(Fate::LabelRouted),
                "a pane-less row resolves by label and must survive any number of sweeps"
            );
            assert!(
                !d.pruned().contains(&"LabelPin"),
                "a pane-less row was PRUNED — the exact bug being replaced"
            );
        }
        assert!(
            !c.contains_key("LabelPin"),
            "a pane-less row must not accumulate confirmations at all"
        );
    }

    /// A pane that comes back resets the evidence rather than leaving a stale count
    /// that would prune it one sweep later.
    #[test]
    fn a_pane_that_returns_resets_its_confirmation_count() {
        let mut c: BTreeMap<String, u32> = BTreeMap::new();
        let gone = herdr(&["w9:p1"]);
        let _ = decide(&rows(), &gone, &mut c, REQUIRED_CONFIRMATIONS);
        let _ = decide(&rows(), &gone, &mut c, REQUIRED_CONFIRMATIONS);
        assert_eq!(c.get("DoomedPin").copied(), Some(2), "two sightings banked");

        let back = herdr(&["w9:p1", "w11:p1"]);
        let d = decide(&rows(), &back, &mut c, REQUIRED_CONFIRMATIONS);
        assert_eq!(d.fate_of("DoomedPin"), Some(Fate::Alive));
        assert!(
            !c.contains_key("DoomedPin"),
            "a returning pane must clear its count, not leave it armed: {c:?}"
        );

        // And it takes a fresh three, not one, to prune it again.
        let d = decide(&rows(), &gone, &mut c, REQUIRED_CONFIRMATIONS);
        assert_eq!(d.fate_of("DoomedPin"), Some(Fate::Unconfirmed));
    }

    /// A prune must leave a record, because a delete has no memory and the registry
    /// has to stay reconstructable weeks later.
    #[test]
    fn a_prune_leaves_a_tombstone_naming_the_pane_and_why() {
        let mut c: BTreeMap<String, u32> = BTreeMap::new();
        let known = herdr(&["w9:p1"]);
        let mut last = Decision {
            fates: vec![],
            tombstones: vec![],
        };
        for _ in 0..3 {
            last = decide(&rows(), &known, &mut c, REQUIRED_CONFIRMATIONS);
        }
        assert_eq!(last.tombstones.len(), 1, "exactly one prune happened");
        let t = &last.tombstones[0];
        assert_eq!(t.pin, "DoomedPin");
        assert_eq!(
            t.pane, "w11:p1",
            "the tombstone must name the pane that died"
        );
        assert_eq!(t.confirmations, REQUIRED_CONFIRMATIONS);
        assert!(
            t.why.contains("consecutive"),
            "the why must say why: {}",
            t.why
        );
    }

    /// `required` is honoured rather than hardcoded, so a caller can widen the
    /// window for a fleet that is known to churn.
    #[test]
    fn the_confirmation_requirement_is_caller_controlled() {
        let mut c: BTreeMap<String, u32> = BTreeMap::new();
        let known = herdr(&["w9:p1"]);
        let d1 = decide(&rows(), &known, &mut c, 1);
        assert_eq!(
            d1.fate_of("DoomedPin"),
            Some(Fate::Pruned),
            "required=1 prunes at once"
        );
    }
}
