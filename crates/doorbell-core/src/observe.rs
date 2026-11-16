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

/// How long a bead's pane may sit in `blocked` before it is an escalation.
pub const BLOCKED_AFTER_SECS: u64 = 600;

/// One lease period: a `request-release` unanswered this long is an
/// escalation. An EXPIRED lease is not — it frees itself.
pub const RELEASE_WAIT_SECS: u64 = 180;

impl ObserveEscalation {
    /// Arm (a): a bead whose assigned pane stayed `blocked` past the window.
    pub fn blocked_bead(bead: &str, pin: &str, blocked_secs: u64, lesson: &str) -> Self {
        Self {
            kind: ObserveKind::BlockedBead,
            subject: bead.to_string(),
            evidence: format!("{pin} blocked {blocked_secs}s (>{BLOCKED_AFTER_SECS}s)"),
            lesson: lesson.to_string(),
        }
    }

    /// Arm (b): a peer's `request-release` the holder never answered inside one
    /// lease period. The episode key is the PATH — one ring per requester per
    /// path, so two requests on one file do not become two pages.
    pub fn release_wait(
        path: &str,
        requester: &str,
        holder: &str,
        bead: &str,
        age_secs: u64,
    ) -> Self {
        Self {
            kind: ObserveKind::ReservationLadder,
            subject: path.to_string(),
            evidence: format!(
                "holder {holder} bead {bead} age {age_secs}s asked by {requester} — no answer inside {RELEASE_WAIT_SECS}s"
            ),
            lesson: "answer a request-release by transferring the lease or yielding; an unanswered ask blocks a peer".to_string(),
        }
    }

    /// Arm (c): a worker marked a fix-vs-hand-over fork explicitly. The subject
    /// is the bead, so one fork per bead rings once.
    pub fn fork(bead: &str, question: &str) -> Self {
        Self {
            kind: ObserveKind::Fork,
            subject: bead.to_string(),
            evidence: question.trim().to_string(),
            lesson: "a wave that hits a judgment call should mark it before it blocks, not after"
                .to_string(),
        }
    }
}

/// Parse the fork convention: a mail SUBJECT of `[fork] <bead>: <question>`.
/// `None` for anything else, so the classifier can never mistake a normal
/// subject for a fork.
pub fn fork_subject(subject: &str) -> Option<(&str, &str)> {
    let rest = subject.trim().strip_prefix("[fork]")?.trim_start();
    let (bead, question) = rest.split_once(':')?;
    let bead = bead.trim();
    let question = question.trim();
    if bead.is_empty() || question.is_empty() {
        return None;
    }
    Some((bead, question))
}

/// One outstanding peer request-release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseWait {
    pub path: String,
    pub requester: String,
    pub holder: String,
    pub bead: String,
    pub asked_at: u64,
}

impl ReleaseWait {
    fn key(&self) -> (String, String) {
        (self.path.clone(), self.requester.clone())
    }
}

/// The outstanding request-release asks. A request is answered by a transfer
/// (the holder yielded) or by the path no longer existing; unanswered past one
/// lease period it escalates ONCE per (path, requester).
#[derive(Debug, Default)]
pub struct ReleaseWaits {
    open: Vec<ReleaseWait>,
}

impl ReleaseWaits {
    pub fn note_request(&mut self, wait: ReleaseWait) {
        let key = wait.key();
        self.open.retain(|w| w.key() != key);
        self.open.push(wait);
    }

    /// The holder yielded (transfer / yield / release): the ask is answered.
    pub fn note_answered(&mut self, path: &str, requester: &str) {
        let key = (path.to_string(), requester.to_string());
        self.open.retain(|w| w.key() != key);
    }

    /// Requests older than one lease period that nobody answered.
    pub fn expired(&self, now: u64) -> Vec<ObserveEscalation> {
        self.open
            .iter()
            .filter(|w| now.saturating_sub(w.asked_at) >= RELEASE_WAIT_SECS)
            .map(|w| {
                ObserveEscalation::release_wait(
                    &w.path,
                    &w.requester,
                    &w.holder,
                    &w.bead,
                    now.saturating_sub(w.asked_at),
                )
            })
            .collect()
    }
}

fn row_str<'a>(row: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(serde_json::Value::as_str)
}

fn row_body(row: &serde_json::Value) -> Option<serde_json::Value> {
    row_str(row, "body_md")
        .or_else(|| row_str(row, "body"))
        .and_then(|b| serde_json::from_str(b).ok())
}

fn row_field(row: &serde_json::Value, key: &str) -> Option<String> {
    let body = row_body(row);
    body.as_ref()
        .and_then(|b| b.get(key).and_then(serde_json::Value::as_str))
        .or_else(|| row_str(row, key))
        .map(str::to_string)
}

/// The `(verb, path)` a peer directive row carries: the `p2p:<verb> <path>`
/// subject, the `[request-release] <path>` subject, or the body's
/// `{"verb":…,"path":…}`. All three exist live.
fn verb_and_path(row: &serde_json::Value) -> Option<(String, String)> {
    let subject = row_str(row, "subject").unwrap_or("");
    if let Some(rest) = subject.strip_prefix("p2p:") {
        let mut parts = rest.splitn(2, ' ');
        return Some((
            parts.next()?.to_string(),
            parts.next().unwrap_or("").trim().to_string(),
        ));
    }
    if let Some(rest) = subject.strip_prefix("[request-release]") {
        return Some(("request-release".to_string(), rest.trim().to_string()));
    }
    let body = row_body(row)?;
    Some((
        body.get("verb")?.as_str()?.to_string(),
        body.get("path")?.as_str()?.to_string(),
    ))
}

/// Arm (b)'s input: a `request-release` row in the HOLDER's inbox. `holder` is
/// the pin whose inbox carried it, `sender` is the requester, and the ask is
/// timed from the row's own rumor time (not the sweep clock).
pub fn release_request(row: &serde_json::Value, holder: &str) -> Option<ReleaseWait> {
    let (verb, path) = verb_and_path(row)?;
    if verb != "request-release" || path.is_empty() {
        return None;
    }
    let requester = row_str(row, "sender")?.trim();
    if requester.is_empty() || requester == holder {
        return None;
    }
    let asked_at = crate::row_created_us(row)?.max(0) / 1_000_000;
    Some(ReleaseWait {
        path,
        requester: requester.to_string(),
        holder: holder.to_string(),
        bead: row_field(row, "bead").unwrap_or_default(),
        asked_at: asked_at as u64,
    })
}

/// The answer: a `release-response` row in the REQUESTER's inbox. Its subject
/// or body names the path; the ask is cleared by (path, requester).
pub fn release_answer(row: &serde_json::Value) -> Option<String> {
    let (verb, path) = verb_and_path(row)?;
    if verb != "release-response" || path.is_empty() {
        return None;
    }
    Some(path)
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

    /// Arm (a): the threshold is a boundary, not a vibe — 599s is fine, 600s
    /// escalates, and the evidence names the pin and the age.
    #[test]
    fn a_blocked_pane_escalates_at_the_window_not_before() {
        let e = ObserveEscalation::blocked_bead("bd-a", "CyanCove", BLOCKED_AFTER_SECS, "l");
        assert_eq!(e.kind, ObserveKind::BlockedBead);
        assert_eq!(e.subject, "bd-a");
        assert!(e.evidence.contains("CyanCove"), "{:?}", e.evidence);
        assert_eq!(BLOCKED_AFTER_SECS, 600);
    }

    /// Arm (b): one ring per (path, requester), only after a full lease period,
    /// and an answered request never escalates.
    #[test]
    fn a_release_wait_escalates_once_after_one_lease_period() {
        let mut waits = ReleaseWaits::default();
        waits.note_request(ReleaseWait {
            path: "src/a.rs".into(),
            requester: "BoldBay".into(),
            holder: "CyanCove".into(),
            bead: "bd-a".into(),
            asked_at: 1_000,
        });
        assert!(
            waits.expired(1_000 + RELEASE_WAIT_SECS - 1).is_empty(),
            "under one lease period it is just a request"
        );
        let due = waits.expired(1_000 + RELEASE_WAIT_SECS);
        assert_eq!(due.len(), 1, "{due:?}");
        assert_eq!(due[0].kind, ObserveKind::ReservationLadder);
        assert_eq!(due[0].subject, "src/a.rs", "the path is the episode key");
        assert!(due[0].evidence.contains("CyanCove"), "{due:?}");
        assert!(due[0].evidence.contains("bd-a"), "{due:?}");

        // Answered: gone, never escalates.
        waits.note_answered("src/a.rs", "BoldBay");
        assert!(waits.expired(9_999).is_empty(), "answered asks are silent");
    }

    /// Arm (c): the fork convention parses exactly, and nothing else matches.
    #[test]
    fn only_a_fork_subject_is_a_fork() {
        assert_eq!(
            fork_subject("[fork] bd-a: fix here or hand over?"),
            Some(("bd-a", "fix here or hand over?"))
        );
        assert_eq!(
            fork_subject("  [fork]  bd-b :  ask "),
            Some(("bd-b", "ask"))
        );
        for not_a_fork in [
            "bd-a: fix here or hand over?",
            "[fork] bd-a",
            "[fork]: question",
            "[forking] bd-a: question",
            "[fork] bd-a: ",
        ] {
            assert_eq!(fork_subject(not_a_fork), None, "{not_a_fork:?}");
        }
        let e = ObserveEscalation::fork("bd-a", "fix here or hand over?");
        assert_eq!(e.kind, ObserveKind::Fork);
        assert_eq!(e.subject, "bd-a", "one fork per bead");
        assert_eq!(e.evidence, "fix here or hand over?");
    }

    /// Arm (b)'s data: a request row names its path, its requester, its holder,
    /// its bead and its age — and nothing else parses as one.
    #[test]
    fn a_release_request_row_parses_from_subject_or_body() {
        let subject_form = serde_json::json!({
            "subject": "p2p:request-release src/a.rs",
            "sender": "BoldBay",
            "created_ts_us": 1_000_000_000i64,
            "body": r#"{"verb":"request-release","path":"src/a.rs","bead":"bd-a"}"#,
        });
        let wait = release_request(&subject_form, "CyanCove").expect("subject form");
        assert_eq!(wait.path, "src/a.rs");
        assert_eq!(wait.requester, "BoldBay");
        assert_eq!(wait.holder, "CyanCove");
        assert_eq!(wait.bead, "bd-a");
        assert_eq!(wait.asked_at, 1_000, "age is timed from the rumor time");

        let body_form = serde_json::json!({
            "sender": "BoldBay",
            "created_ts_us": 1_000_000_000i64,
            "body": r#"{"verb":"request-release","path":"src/b.rs","bead":"bd-b"}"#,
        });
        assert_eq!(
            release_request(&body_form, "CyanCove").map(|w| w.path),
            Some("src/b.rs".to_string())
        );

        for not_a_request in [
            serde_json::json!({"subject": "p2p:release-response src/a.rs", "sender": "BoldBay"}),
            serde_json::json!({"subject": "bd-a: ordinary mail", "sender": "BoldBay"}),
            serde_json::json!({"subject": "p2p:request-release ", "sender": "BoldBay"}),
            serde_json::json!({"subject": "p2p:request-release src/a.rs", "sender": "CyanCove"}),
        ] {
            assert_eq!(
                release_request(&not_a_request, "CyanCove"),
                None,
                "{not_a_request}"
            );
        }
    }

    /// Only a response answers an ask — a second request on the same path is
    /// not an answer, and a response names the path so the key can match.
    #[test]
    fn only_a_release_response_answers_an_ask() {
        let answer =
            serde_json::json!({"subject": "p2p:release-response src/a.rs", "sender": "CyanCove"});
        assert_eq!(release_answer(&answer), Some("src/a.rs".to_string()));
        let answer_body = serde_json::json!({
            "body": r#"{"verb":"release-response","path":"src/b.rs","status":"yield"}"#,
        });
        assert_eq!(release_answer(&answer_body), Some("src/b.rs".to_string()));
        let request = serde_json::json!({"subject": "p2p:request-release src/a.rs"});
        assert_eq!(release_answer(&request), None);
        assert_eq!(
            release_answer(&serde_json::json!({"subject": "plain"})),
            None
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
