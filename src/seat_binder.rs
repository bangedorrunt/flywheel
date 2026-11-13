//! Pure step-to-seat binder (ADR-0022 D8).
//!
//! Answers "which live seat serves this step". Decision only: no wiring into
//! the engine or the doorbell, no I/O. The reconciler (D7) and the SeatMachine
//! come later. Each binding is journaled with the step's effect key elsewhere;
//! journaling is NOT in this slice.

use crate::stages::StageContext;

/// A step's needs, from its stage contract with step overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepNeed {
    pub step: String,
    pub role: String,
    pub harness: Option<String>,
    pub model: Option<String>,
    /// StageMode vocabulary (`read-only` | `workspace-write` | `isolated`).
    pub mode: Option<String>,
    pub context: StageContext,
    /// Pin that authored the work a review/verify step checks.
    pub reviews_author: Option<String>,
}

/// A live seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSeat {
    pub pin: String,
    pub role: String,
    pub harness: String,
    pub model: Option<String>,
    /// StageMode vocabulary (`read-only` | `workspace-write` | `isolated`).
    pub mode: Option<String>,
    pub idle: bool,
    /// The harness can start a new session in the same pane.
    pub fresh_session: bool,
}

/// Binder policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindPolicy {
    pub crew_max: usize,
    pub headless_allowed: bool,
}

/// Binding outcome, first match wins in D8 table order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    Reuse { pin: String },
    FreshSession { pin: String },
    Other { pin: String },
    Headless,
    Spawn,
    Patch { pin: String, fields: Vec<String> },
    Wait { reason: String },
}

fn matches(need: &StepNeed, seat: &LiveSeat) -> bool {
    if need.role != seat.role {
        return false;
    }
    if let Some(h) = &need.harness {
        if seat.harness != *h {
            return false;
        }
    }
    if let Some(m) = &need.model {
        if seat.model.as_deref() != Some(m.as_str()) {
            return false;
        }
    }
    if let Some(mode) = &need.mode {
        if seat.mode.as_deref() != Some(mode.as_str()) {
            return false;
        }
    }
    true
}

fn is_author(need: &StepNeed, pin: &str) -> bool {
    need.reviews_author.as_deref() == Some(pin)
}

fn diff_fields(need: &StepNeed, seat: &LiveSeat) -> Vec<String> {
    let mut fields = Vec::new();
    if let Some(h) = &need.harness {
        if seat.harness != *h {
            fields.push("harness".to_string());
        }
    }
    if let Some(m) = &need.model {
        if seat.model.as_deref() != Some(m.as_str()) {
            fields.push("model".to_string());
        }
    }
    if let Some(mode) = &need.mode {
        if seat.mode.as_deref() != Some(mode.as_str()) {
            fields.push("mode".to_string());
        }
    }
    fields
}

/// Pure fold: which live seat serves this step. Candidates are pin-ordered,
/// so a replay binds the same way. Never borrows another role's seat.
pub fn bind(need: &StepNeed, seats: &[LiveSeat], policy: &BindPolicy) -> Binding {
    let mut same_role: Vec<&LiveSeat> = seats.iter().filter(|s| s.role == need.role).collect();
    same_role.sort_by(|a, b| a.pin.cmp(&b.pin));

    let non_author: Vec<&LiveSeat> = same_role
        .iter()
        .filter(|s| !is_author(need, &s.pin))
        .copied()
        .collect();

    let idle_matching: Vec<&LiveSeat> = non_author
        .iter()
        .filter(|s| s.idle && matches(need, s))
        .copied()
        .collect();

    if need.context == StageContext::Warm {
        if let Some(seat) = idle_matching.first() {
            return Binding::Reuse {
                pin: seat.pin.clone(),
            };
        }
    }

    if need.context == StageContext::Fresh {
        if let Some(seat) = idle_matching.iter().find(|s| s.fresh_session) {
            return Binding::FreshSession {
                pin: seat.pin.clone(),
            };
        }
        if let Some(seat) = idle_matching.first() {
            return Binding::Other {
                pin: seat.pin.clone(),
            };
        }
        if policy.headless_allowed {
            return Binding::Headless;
        }
        if seats.len() < policy.crew_max {
            return Binding::Spawn;
        }
    }

    if !non_author.is_empty() && idle_matching.is_empty() {
        let mut mismatched: Vec<(&LiveSeat, Vec<String>)> = Vec::new();
        for seat in &non_author {
            let fields = diff_fields(need, seat);
            if !fields.is_empty() {
                mismatched.push((seat, fields));
            }
        }
        if mismatched.len() == non_author.len() {
            let (seat, fields) = &mismatched[0];
            return Binding::Patch {
                pin: seat.pin.clone(),
                fields: fields.clone(),
            };
        }
    }

    if need.context == StageContext::Warm {
        if policy.headless_allowed {
            return Binding::Headless;
        }
        if seats.len() < policy.crew_max {
            return Binding::Spawn;
        }
    }

    Binding::Wait {
        reason: "no seat within crew_max".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::StageContext;

    fn need(role: &str, context: StageContext) -> StepNeed {
        StepNeed {
            step: "s".to_string(),
            role: role.to_string(),
            harness: Some("h".to_string()),
            model: None,
            mode: None,
            context,
            reviews_author: None,
        }
    }

    fn seat(pin: &str, role: &str, idle: bool, fresh: bool) -> LiveSeat {
        LiveSeat {
            pin: pin.to_string(),
            role: role.to_string(),
            harness: "h".to_string(),
            model: None,
            mode: None,
            idle,
            fresh_session: fresh,
        }
    }

    fn policy(max: usize, headless: bool) -> BindPolicy {
        BindPolicy {
            crew_max: max,
            headless_allowed: headless,
        }
    }

    #[test]
    fn seat_binder_warm_idle_reuse() {
        let need = need("coder", StageContext::Warm);
        let seats = vec![
            seat("b", "coder", true, false),
            seat("a", "coder", true, false),
        ];
        assert_eq!(
            bind(&need, &seats, &policy(5, false)),
            Binding::Reuse {
                pin: "a".to_string()
            }
        );
    }

    #[test]
    fn seat_binder_busy_seat_not_reused() {
        let need = need("coder", StageContext::Warm);
        let seats = vec![seat("a", "coder", false, false)];
        assert_ne!(
            bind(&need, &seats, &policy(1, false)),
            Binding::Reuse {
                pin: "a".to_string()
            }
        );
        assert_eq!(
            bind(&need, &seats, &policy(1, false)),
            Binding::Wait {
                reason: "no seat within crew_max".to_string()
            }
        );
    }

    #[test]
    fn seat_binder_fresh_with_session_gives_new_session() {
        let need = need("coder", StageContext::Fresh);
        let seats = vec![seat("a", "coder", true, true)];
        assert_eq!(
            bind(&need, &seats, &policy(5, false)),
            Binding::FreshSession {
                pin: "a".to_string()
            }
        );
    }

    #[test]
    fn seat_binder_fresh_without_session_prefers_other_idle() {
        let need = need("coder", StageContext::Fresh);
        let seats = vec![
            seat("a", "coder", true, false),
            seat("b", "coder", true, false),
        ];
        assert_eq!(
            bind(&need, &seats, &policy(5, false)),
            Binding::Other {
                pin: "a".to_string()
            }
        );
    }

    #[test]
    fn seat_binder_fresh_without_idle_gives_headless_when_allowed() {
        let need = need("coder", StageContext::Fresh);
        let seats = vec![LiveSeat {
            pin: "a".to_string(),
            role: "coder".to_string(),
            harness: "h".to_string(),
            model: None,
            mode: None,
            idle: false,
            fresh_session: false,
        }];
        assert_eq!(bind(&need, &seats, &policy(5, true)), Binding::Headless);
    }

    #[test]
    fn seat_binder_fresh_without_idle_spawns_under_crew_max() {
        let need = need("coder", StageContext::Fresh);
        let seats = vec![LiveSeat {
            pin: "a".to_string(),
            role: "coder".to_string(),
            harness: "h".to_string(),
            model: None,
            mode: None,
            idle: false,
            fresh_session: false,
        }];
        assert_eq!(bind(&need, &seats, &policy(2, false)), Binding::Spawn);
    }

    #[test]
    fn seat_binder_reviewer_never_author() {
        let mut need = need("reviewer", StageContext::Warm);
        need.reviews_author = Some("a".to_string());
        let seats = vec![seat("a", "reviewer", true, false)];
        let got = bind(&need, &seats, &policy(2, false));
        assert_ne!(
            got,
            Binding::Reuse {
                pin: "a".to_string()
            }
        );
        assert_eq!(got, Binding::Spawn);
    }

    #[test]
    fn seat_binder_mismatch_gives_patch_naming_fields() {
        let need = StepNeed {
            step: "s".to_string(),
            role: "coder".to_string(),
            harness: Some("h2".to_string()),
            model: Some("m2".to_string()),
            mode: Some("isolated".to_string()),
            context: StageContext::Warm,
            reviews_author: None,
        };
        let seats = vec![LiveSeat {
            pin: "a".to_string(),
            role: "coder".to_string(),
            harness: "h1".to_string(),
            model: Some("m1".to_string()),
            mode: Some("read-only".to_string()),
            idle: true,
            fresh_session: false,
        }];
        assert_eq!(
            bind(&need, &seats, &policy(5, false)),
            Binding::Patch {
                pin: "a".to_string(),
                fields: vec![
                    "harness".to_string(),
                    "model".to_string(),
                    "mode".to_string()
                ]
            }
        );
    }

    #[test]
    fn seat_binder_at_crew_max_waits() {
        let need = need("coder", StageContext::Fresh);
        let seats = vec![LiveSeat {
            pin: "a".to_string(),
            role: "coder".to_string(),
            harness: "h".to_string(),
            model: None,
            mode: None,
            idle: false,
            fresh_session: false,
        }];
        assert_eq!(
            bind(&need, &seats, &policy(1, false)),
            Binding::Wait {
                reason: "no seat within crew_max".to_string()
            }
        );
    }

    #[test]
    fn seat_binder_never_borrows_other_role() {
        let need = need("coder", StageContext::Warm);
        let seats = vec![seat("b", "reviewer", true, false)];
        assert_eq!(
            bind(&need, &seats, &policy(1, false)),
            Binding::Wait {
                reason: "no seat within crew_max".to_string()
            }
        );
    }

    #[test]
    fn seat_binder_pin_order_deterministic() {
        let need = need("coder", StageContext::Warm);
        let a_first = vec![
            seat("a", "coder", true, false),
            seat("b", "coder", true, false),
        ];
        let b_first = vec![
            seat("b", "coder", true, false),
            seat("a", "coder", true, false),
        ];
        assert_eq!(
            bind(&need, &a_first, &policy(5, false)),
            bind(&need, &b_first, &policy(5, false))
        );
    }
}
