// governed-by: ADR-0011
// governed-by: ADR-0029
//! Deterministic rot scorer (ADR-0029 Wave 1 / ADR-0011 D7).
//!
//! Pure: no filesystem, no clock, no env, no network. Identical events ⇒
//! identical verdict. Bytes are a named proxy for tokens.

use serde::{Deserialize, Serialize};

pub const ADVISE_AT: u32 = 40;
pub const COMPACT_AT: u32 = 60;
pub const RESTART_AT: u32 = 80;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RotEvents {
    /// pane output bytes (named proxy for tokens)
    pub bytes: u64,
    pub turns: u32,
    pub floor: u64,
    pub ceiling: u64,
    /// 0 when no structured transcript exists (skip, logged by caller)
    pub failure_spiral: u32,
    /// count of identical consecutive hashed tail windows
    pub repetition: u32,
    /// dispatched bead with no [bd-*] lifecycle mail for N turns (0 = N/A)
    pub marker_silence_turns: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RotVerdict {
    Healthy,
    Advise,
    Compact,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotScore {
    pub score: u32,
    pub verdict: RotVerdict,
}

/// ADR-0011 D7 / ADR-0029 Wave 5: interrupt only for the first 3 nudges.
/// 4th+ still delivers as mail; the pane is never interrupted until progress.
pub fn interrupt_nudge(streak: u32) -> bool {
    streak < 3
}

/// Map a rot score onto a loop action (pure). Caller emits Respawn/Send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotAction {
    None,
    Advise,
    Compact,
    Respawn,
}

pub fn action(ev: &RotEvents) -> RotAction {
    match score(ev).verdict {
        RotVerdict::Healthy => RotAction::None,
        RotVerdict::Advise => RotAction::Advise,
        RotVerdict::Compact => RotAction::Compact,
        RotVerdict::Restart => RotAction::Respawn,
    }
}

pub fn compact_prompt() -> &'static str {
    "ROT compact: summarize working state to a file (file-first) and continue lean. Do not restart unless you cannot proceed."
}

pub fn score(ev: &RotEvents) -> RotScore {
    let mut score: u32 = 0;
    if ev.repetition >= 3 {
        score = score.saturating_add(30);
    }
    if ev.marker_silence_turns >= 3 {
        score = score.saturating_add(30);
    }
    if ev.failure_spiral > 0 {
        score = score.saturating_add(40.min(ev.failure_spiral.saturating_mul(10)));
    }
    let ceiling_hit = ev.ceiling > 0 && ev.bytes >= ev.ceiling;
    let mut verdict = if score >= RESTART_AT {
        RotVerdict::Restart
    } else if score >= COMPACT_AT {
        RotVerdict::Compact
    } else if score >= ADVISE_AT {
        RotVerdict::Advise
    } else {
        RotVerdict::Healthy
    };
    if ceiling_hit && score >= COMPACT_AT {
        verdict = RotVerdict::Restart;
    }
    RotScore { score, verdict }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_events_identical_verdict() {
        let ev = RotEvents {
            bytes: 1000,
            turns: 4,
            floor: 0,
            ceiling: 10_000,
            failure_spiral: 0,
            repetition: 3,
            marker_silence_turns: 3,
        };
        let a = score(&ev);
        let b = score(&ev);
        assert_eq!(a, b);
        assert_eq!(a.score, 60);
        assert_eq!(a.verdict, RotVerdict::Compact);
    }

    #[test]
    fn advise_never_restarts() {
        // advise_at=40: failure_spiral 4 × 10 = 40. Repetition alone is 30 = Healthy.
        let ev = RotEvents {
            failure_spiral: 4,
            ..RotEvents::default()
        };
        let s = score(&ev);
        assert_eq!(s.verdict, RotVerdict::Advise);
        assert!(s.score < RESTART_AT);
    }

    #[test]
    fn ceiling_plus_compact_escalates_restart() {
        let ev = RotEvents {
            bytes: 9000,
            ceiling: 8000,
            repetition: 3,
            marker_silence_turns: 3,
            ..RotEvents::default()
        };
        assert_eq!(score(&ev).verdict, RotVerdict::Restart);
        assert_eq!(action(&ev), RotAction::Respawn);
    }

    #[test]
    fn nudge_cap_three_then_mail_only() {
        assert!(interrupt_nudge(0));
        assert!(interrupt_nudge(2));
        assert!(!interrupt_nudge(3));
        assert!(!interrupt_nudge(9));
    }

    #[test]
    fn advise_is_not_a_respawn() {
        let ev = RotEvents {
            failure_spiral: 4,
            ..RotEvents::default()
        };
        assert_eq!(action(&ev), RotAction::Advise);
    }
}
