// bd-9os20: the pure half of the Claude wake module. Given a decoded wake and
// the seat's current state, decide ONE delivery action. No IO, no clock, no
// env: the plugin owns the effects (claim, prompt.submit, session.append) and
// calls this through `flywheel __hook`, so the decision is witnessable in a
// unit test and cannot drift from what the binary tells the plugin.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Empty prompt box: the engine starts a new turn.
    Submit,
    /// Urgent wake into a running turn, after the current tool call.
    Append,
    /// Native path won, or the seat cannot receive notices: stay silent.
    Drop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub action: Action,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct Wake {
    pub urgent: bool,
    /// A subagent session never receives notices (bd-9os20).
    pub subagent: bool,
    /// True when the prompt box is empty / no turn is running.
    pub idle: bool,
    /// True only for the first claimer of this bus event id (bd-tdpix).
    pub claimed: bool,
    /// Consecutive failed delivery checks for this seat.
    pub failures: u8,
}

pub const MAX_FAILURES: u8 = 3;

pub fn status_healthy() -> String {
    "flywheel wake delivery active".into()
}

pub fn status_stopped(failures: u8) -> String {
    format!("flywheel wake delivery stopped: {failures} failed checks (check toron MCP)")
}

/// The one decision function. Ordered guards matter: a lost claim or a
/// subagent drops before urgency is even consulted, and a stopped delivery
/// never resumes on its own.
pub fn decide(wake: &Wake) -> Decision {
    let healthy = wake.failures < MAX_FAILURES && !wake.subagent && wake.claimed;
    let action = if !healthy {
        Action::Drop
    } else if wake.urgent {
        // The engine appends into the running turn; when idle the same append
        // is still a valid first turn, so urgency never submits.
        Action::Append
    } else if wake.idle {
        Action::Submit
    } else {
        // A normal wake while busy stays with the doorbell's queue; the
        // plugin must never type into an occupied input box.
        Action::Drop
    };
    let status = if wake.subagent {
        "flywheel wake delivery: subagents do not receive notices".into()
    } else if wake.failures >= MAX_FAILURES {
        status_stopped(wake.failures)
    } else {
        status_healthy()
    };
    Decision { action, status }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wake(urgent: bool, claimed: bool, failures: u8, subagent: bool) -> Wake {
        Wake {
            urgent,
            subagent,
            idle: true,
            claimed,
            failures,
        }
    }

    #[test]
    fn a_lost_claim_never_delivers() {
        assert_eq!(decide(&wake(false, false, 0, false)).action, Action::Drop);
        assert_eq!(decide(&wake(true, false, 0, false)).action, Action::Drop);
    }

    #[test]
    fn subagents_never_receive_notices() {
        let d = decide(&wake(true, true, 0, true));
        assert_eq!(d.action, Action::Drop);
        assert!(d.status.contains("subagents do not receive notices"));
    }

    #[test]
    fn normal_mail_submits_and_urgent_appends() {
        assert_eq!(decide(&wake(false, true, 0, false)).action, Action::Submit);
        assert_eq!(decide(&wake(true, true, 0, false)).action, Action::Append);
    }

    #[test]
    fn delivery_stops_after_three_failed_checks() {
        for failures in [0, 1, 2] {
            assert_ne!(
                decide(&wake(false, true, failures, false)).action,
                Action::Drop
            );
        }
        let stopped = decide(&wake(true, true, MAX_FAILURES, false));
        assert_eq!(stopped.action, Action::Drop);
        assert_eq!(stopped.status, status_stopped(MAX_FAILURES));
    }

    #[test]
    fn normal_mail_while_busy_is_left_to_the_doorbell() {
        let mut w = wake(false, true, 0, false);
        w.idle = false;
        assert_eq!(decide(&w).action, Action::Drop);
        w.urgent = true;
        assert_eq!(decide(&w).action, Action::Append);
    }

    #[test]
    fn healthy_status_does_not_mention_failure() {
        assert!(!status_healthy().contains("stopped"));
    }
}
