//! Crew briefs (bd-0uzrq.4): a dispatch that expects ONE structured reply.
//!
//! Live failure: crews piled up unread backlogs, "ack" meant read rather than
//! handled, and a done mail was free text matched by eye. A brief carries an
//! id; the crew's `crew done <brief> --receipt` references it, which closes the
//! brief and wakes the captain exactly once. Open briefs stay visible with
//! their age, so a silent crew is a row, not a memory.
//!
//! The reducer is pure: `(book, msg) -> effects`. The CLI owns persistence and
//! the wake, so a replayed reply closes nothing twice and rings no one twice.

use serde::{Deserialize, Serialize};

/// One brief: the id the crew replies with, who it went to, and when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub id: String,
    pub pin: String,
    pub subject: String,
    pub opened_ts_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_ts_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    /// bd-bi0mv: true when `receipt` is canonical typed JSON rather than prose.
    #[serde(default, skip_serializing_if = "is_false")]
    pub receipt_typed: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// The book, keyed by id in order of opening.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefBook {
    #[serde(default)]
    pub briefs: Vec<Brief>,
}

/// What can happen to the book.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    Opened(Brief),
    /// The crew's structured reply. `receipt` is what proved the work.
    Done {
        id: String,
        receipt: String,
        now_us: u64,
    },
}

/// What the interpreter must do after a message. The captain wake is the only
/// effect: closing a brief is the moment the captain must look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    WakeCaptain { id: String, pin: String },
}

/// One open brief as `crew inbox --open` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpenBrief {
    pub id: String,
    pub pin: String,
    pub subject: String,
    pub age_secs: u64,
}

impl BriefBook {
    pub fn reduce(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Opened(brief) => {
                if self.briefs.iter().any(|b| b.id == brief.id) {
                    // A replayed open is not a second brief.
                    return Vec::new();
                }
                self.briefs.push(brief);
                Vec::new()
            }
            Msg::Done {
                id,
                receipt,
                now_us,
            } => {
                let Some(brief) = self.briefs.iter_mut().find(|b| b.id == id) else {
                    return Vec::new();
                };
                // A replayed done reply (or one for an already-closed brief)
                // closes nothing twice and rings no one twice.
                if brief.closed_ts_us.is_some() {
                    return Vec::new();
                }
                brief.closed_ts_us = Some(now_us);
                brief.receipt = Some(receipt);
                vec![Effect::WakeCaptain {
                    id: brief.id.clone(),
                    pin: brief.pin.clone(),
                }]
            }
        }
    }

    /// Every brief with no reply yet, oldest first, with its age. `pin`
    /// narrows to one crew: "what is outstanding for THIS pin".
    pub fn open_for(&self, pin: Option<&str>, now_us: u64) -> Vec<OpenBrief> {
        let mut rows: Vec<OpenBrief> = self
            .briefs
            .iter()
            .filter(|b| pin.is_none_or(|p| b.pin == p))
            .filter(|b| b.closed_ts_us.is_none())
            .map(|b| OpenBrief {
                id: b.id.clone(),
                pin: b.pin.clone(),
                subject: b.subject.clone(),
                age_secs: now_us.saturating_sub(b.opened_ts_us) / 1_000_000,
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.age_secs));
        rows
    }

    pub fn get(&self, id: &str) -> Option<&Brief> {
        self.briefs.iter().find(|b| b.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief(id: &str, pin: &str, opened_ts_us: u64) -> Brief {
        Brief {
            id: id.to_string(),
            pin: pin.to_string(),
            subject: format!("brief {id}"),
            opened_ts_us,
            closed_ts_us: None,
            receipt: None,
            receipt_typed: false,
        }
    }

    /// A brief with no reply ages into the open list — that is how a silent
    /// crew becomes a row the captain can see instead of a memory.
    #[test]
    fn an_unanswered_brief_ages_into_the_open_list() {
        let mut book = BriefBook::default();
        book.reduce(Msg::Opened(brief("b1", "FrostyCreek", 1_000_000_000)));
        let open = book.open_for(None, 1_000_000_000 + 90 * 1_000_000);
        assert_eq!(open.len(), 1, "{open:?}");
        assert_eq!(open[0].pin, "FrostyCreek");
        assert_eq!(open[0].age_secs, 90);
    }

    /// The structured reply closes the brief and emits EXACTLY ONE captain
    /// wake.
    #[test]
    fn a_done_reply_closes_the_brief_and_wakes_the_captain_once() {
        let mut book = BriefBook::default();
        book.reduce(Msg::Opened(brief("b1", "FrostyCreek", 1)));
        let effects = book.reduce(Msg::Done {
            id: "b1".into(),
            receipt: "commit abc123, proof green".into(),
            now_us: 5,
        });
        assert_eq!(
            effects,
            vec![Effect::WakeCaptain {
                id: "b1".into(),
                pin: "FrostyCreek".into()
            }]
        );
        assert!(
            book.open_for(None, 10).is_empty(),
            "a closed brief leaves the open list"
        );
        assert_eq!(
            book.get("b1").and_then(|b| b.receipt.as_deref()),
            Some("commit abc123, proof green")
        );
    }

    /// A replayed reply closes nothing twice and rings no one twice.
    #[test]
    fn a_replayed_reply_emits_nothing() {
        let mut book = BriefBook::default();
        book.reduce(Msg::Opened(brief("b1", "FrostyCreek", 1)));
        let first = book.reduce(Msg::Done {
            id: "b1".into(),
            receipt: "once".into(),
            now_us: 5,
        });
        assert_eq!(first.len(), 1);
        let replay = book.reduce(Msg::Done {
            id: "b1".into(),
            receipt: "again".into(),
            now_us: 9,
        });
        assert!(
            replay.is_empty(),
            "a replay must not ring the captain again"
        );
        // The first receipt is the record; a replay never rewrites it.
        assert_eq!(
            book.get("b1").and_then(|b| b.receipt.as_deref()),
            Some("once")
        );
        assert_eq!(book.get("b1").and_then(|b| b.closed_ts_us), Some(5));
    }

    /// A reply for an id nobody dispatched is not a close (and not a wake).
    #[test]
    fn a_reply_for_an_unknown_brief_does_nothing() {
        let mut book = BriefBook::default();
        let effects = book.reduce(Msg::Done {
            id: "nope".into(),
            receipt: "x".into(),
            now_us: 5,
        });
        assert!(effects.is_empty());
        assert!(book.open_for(None, 5).is_empty());
    }

    /// `crew inbox --pin` answers "what is outstanding for THIS crew" and
    /// nothing else.
    #[test]
    fn the_open_list_filters_to_one_pin() {
        let mut book = BriefBook::default();
        book.reduce(Msg::Opened(brief("a1", "FrostyCreek", 1)));
        book.reduce(Msg::Opened(brief("a2", "BoldBay", 1)));
        book.reduce(Msg::Opened(brief("a3", "FrostyCreek", 1)));
        assert_eq!(
            book.open_for(None, 2).len(),
            3,
            "no filter lists every crew"
        );
        let only = book.open_for(Some("FrostyCreek"), 2);
        assert_eq!(
            only.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["a1", "a3"]
        );
        assert!(
            book.open_for(Some("Nobody"), 2).is_empty(),
            "an unknown pin has no briefs"
        );
    }
}
