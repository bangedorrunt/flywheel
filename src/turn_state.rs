// governed-by: ADR-0039
//! bd-b2kaa: hook-reported turn state and continuation policy. Pure decisions
//! and small file ports; no clock, env or daemon here (the caller supplies the
//! timestamp), so every rule is witnessable with a fake.

use serde::{Deserialize, Serialize};

/// A hook report is authoritative only while it is younger than this. Past
/// that the doorbell falls back to its screen heuristics.
pub const FRESH_SECS: i64 = 10;
/// A Stop hook may continue a turn at most this many consecutive times.
pub const MAX_CONTINUATIONS: u32 = 3;
/// PostToolUse checks urgent mail no more often than this.
pub const URGENT_CHECK_SECS: i64 = 15;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnReport {
    pub running: bool,
    pub at_us: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuationState {
    pub count: u32,
    pub last_urgent_check_us: i64,
}

/// A fresh hook report beats the screen-derived status; a stale one is ignored
/// so a dead hook cannot pin a seat "working" forever.
pub fn status_from_turn(status: &str, turn: Option<&TurnReport>, now_us: i64) -> String {
    match turn {
        Some(t) if now_us.saturating_sub(t.at_us) <= FRESH_SECS * 1_000_000 => {
            if t.running {
                "working".to_string()
            } else {
                "idle".to_string()
            }
        }
        _ => status.to_string(),
    }
}

/// The Stop hook continues with ONE plain-language line while mail waits, up
/// to `MAX_CONTINUATIONS` consecutive times. Beyond the cap it stays silent.
pub fn stop_line(mail_waiting: bool, state: &ContinuationState) -> Option<&'static str> {
    if mail_waiting && state.count < MAX_CONTINUATIONS {
        Some("mail is waiting — run `toron next` to read it")
    } else {
        None
    }
}

/// PostToolUse emits an urgent notice only when urgent mail is pending AND the
/// 15s throttle has elapsed. The caller advances `last_urgent_check_us`.
pub fn urgent_due(urgent: bool, now_us: i64, state: &ContinuationState) -> bool {
    urgent && now_us.saturating_sub(state.last_urgent_check_us) >= URGENT_CHECK_SECS * 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_report_wins_but_a_stale_one_does_not() {
        assert_eq!(status_from_turn("idle", None, 20_000_000), "idle");
        let fresh = TurnReport {
            running: true,
            at_us: 16_000_000,
        };
        assert_eq!(
            status_from_turn("idle", Some(&fresh), 20_000_000),
            "working"
        );
        let stale = TurnReport {
            running: true,
            at_us: 1_000_000,
        };
        assert_eq!(status_from_turn("idle", Some(&stale), 20_000_000), "idle");
    }

    #[test]
    fn stop_continues_at_most_three_times() {
        for count in 0..3 {
            assert!(
                stop_line(
                    true,
                    &ContinuationState {
                        count,
                        last_urgent_check_us: 0
                    }
                )
                .is_some()
            );
        }
        assert!(
            stop_line(
                true,
                &ContinuationState {
                    count: 3,
                    last_urgent_check_us: 0
                }
            )
            .is_none()
        );
        assert!(stop_line(false, &ContinuationState::default()).is_none());
    }

    #[test]
    fn urgent_check_is_throttled_to_15_seconds() {
        let state = ContinuationState {
            count: 0,
            last_urgent_check_us: 100_000_000,
        };
        assert!(!urgent_due(true, 114_000_000, &state));
        assert!(urgent_due(true, 115_000_000, &state));
        assert!(!urgent_due(false, 200_000_000, &state));
    }
}
