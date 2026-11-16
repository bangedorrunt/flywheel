//! Observe escalations (ADR-0021 D4.5, bd-jhz8x.2): the three arms plus the
//! lesson line, as data the fold can carry.
//!
//! `wave-observe` was a planner seat that watched a channel and mailed the
//! captain. D4.5 moves it into the engine, and the engine's version must be
//! made of signals it can actually see — so this module is the carrier and the
//! dedup rule, and each arm supplies its own subject.
//!
//! The lesson line is not decoration: ADR-0009's failure path requires every
//! escalation to say what a future wave could do differently, and a fold that
//! forgets it silently loses the corpus entry.

use std::collections::BTreeSet;

/// The three arms `wave-observe` carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObserveKind {
    /// A bead's pane sat in `blocked` for longer than the window.
    BlockedBead,
    /// A reservation ladder timed out (request-release unanswered).
    ReservationLadder,
    /// A fix-vs-hand-over fork: the wave cannot decide and must ask.
    Fork,
}

impl ObserveKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BlockedBead => "blocked-bead",
            Self::ReservationLadder => "reservation-ladder",
            Self::Fork => "fork",
        }
    }
}

/// One escalation, ready to post. `subject` is the dedup key with the kind
/// (the bead id, the path, the fork id), `evidence` is what the captain reads
/// first, `lesson` is the ADR-0009 line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ObserveEscalation {
    pub kind: ObserveKind,
    pub subject: String,
    pub evidence: String,
    pub lesson: String,
}

impl ObserveEscalation {
    /// The one line the captain sees. Always ends with `lesson: …` — an
    /// escalation without a lesson is a report, and this format refuses it.
    pub fn line(&self) -> String {
        format!(
            "[observe] {} {}: {} · lesson: {}",
            self.kind.as_str(),
            self.subject,
            self.evidence.trim(),
            self.lesson.trim()
        )
    }

    fn key(&self) -> (ObserveKind, String) {
        (self.kind, self.subject.clone())
    }
}

/// One escalation per (kind, subject) episode. A subject that stops escalating
/// clears its key, so a recurrence rings again — the same shape as the
/// awareness digest's episode rule, and the reason a fold restart cannot page
/// the captain twice for one blocked bead.
#[derive(Debug, Default)]
pub struct EscalateSet {
    live: BTreeSet<(ObserveKind, String)>,
}

impl EscalateSet {
    /// Fold this sweep's escalations; return only the NEW ones. Subjects that
    /// are no longer escalating are forgotten in the same pass.
    pub fn new_since<'a>(
        &mut self,
        escalations: &'a [ObserveEscalation],
    ) -> Vec<&'a ObserveEscalation> {
        let current: BTreeSet<(ObserveKind, String)> =
            escalations.iter().map(ObserveEscalation::key).collect();
        self.live.retain(|k| current.contains(k));
        let mut fresh = Vec::new();
        for e in escalations {
            if self.live.insert(e.key()) {
                fresh.push(e);
            }
        }
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc(kind: ObserveKind, subject: &str, lesson: &str) -> ObserveEscalation {
        ObserveEscalation {
            kind,
            subject: subject.to_string(),
            evidence: format!("evidence for {subject}"),
            lesson: lesson.to_string(),
        }
    }

    /// Every escalation line ends with the lesson line — ADR-0009's failure
    /// path, and the piece a fold port most easily drops.
    #[test]
    fn every_escalation_line_carries_its_lesson() {
        for kind in [
            ObserveKind::BlockedBead,
            ObserveKind::ReservationLadder,
            ObserveKind::Fork,
        ] {
            let line = esc(kind, "bd-a", "split the bead before it blocks").line();
            assert!(line.starts_with("[observe] "), "{line}");
            assert!(
                line.ends_with("lesson: split the bead before it blocks"),
                "{line}"
            );
        }
    }

    /// One ring per episode: the same blocked bead does not page every sweep,
    /// and a bead that unblocks and blocks again does ring again.
    #[test]
    fn a_blocked_bead_rings_once_per_episode() {
        let mut set = EscalateSet::default();
        let e = esc(ObserveKind::BlockedBead, "bd-a", "unblock it earlier");
        assert_eq!(
            set.new_since(std::slice::from_ref(&e)).len(),
            1,
            "first sighting rings"
        );
        assert!(
            set.new_since(std::slice::from_ref(&e)).is_empty(),
            "the same episode is silent on the next sweep"
        );
        assert!(set.new_since(&[]).is_empty(), "resolved: nothing to say");
        assert_eq!(
            set.new_since(std::slice::from_ref(&e)).len(),
            1,
            "a recurrence after resolution rings again"
        );
    }

    /// Distinct subjects are distinct episodes, and the kind is part of the key
    /// (a fork on bd-a is not the same event as a block on bd-a).
    #[test]
    fn kind_and_subject_are_both_part_of_the_episode_key() {
        let mut set = EscalateSet::default();
        let first = vec![
            esc(ObserveKind::BlockedBead, "bd-a", "l"),
            esc(ObserveKind::Fork, "bd-a", "l"),
            esc(ObserveKind::BlockedBead, "bd-b", "l"),
        ];
        assert_eq!(set.new_since(&first).len(), 3, "three distinct episodes");
        assert!(set.new_since(&first).is_empty());
    }
}
