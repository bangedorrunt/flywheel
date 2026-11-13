//! Plan lifecycle statechart (ADR-0022 §3.1).
//!
//! Intent to Confirmed or Abandoned. Pure fold; commands are descriptive
//! only. Closed [`JevOutcome`] mirrors toron failure modes; V2 never blocks
//! Resolve on Jev failure, V3 never lets a blocked seat reach Confirmed.

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

use crate::swarm_plan::{ConsentItem, ConsentReason};

/// Jev outcome for a Resolve. Closed: Ok completes clean, any other outcome
/// degrades (sets `jev_degraded`) but still leaves Drafting — V2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JevOutcome {
    Ok,
    Timeout,
    RateLimited,
    MissingKey,
    Shrug,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PlanState {
    Drafting,
    Resolved,
    AwaitingConsent,
    Confirmed,
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanEvent {
    Resolve {
        needs_consent: Vec<ConsentItem>,
        jev: JevOutcome,
    },
    Patch {
        fields: Vec<String>,
    },
    Approve,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanContext {
    pub yes: bool,
    pub needs_consent: Vec<ConsentItem>,
    pub jev_degraded: bool,
}

impl Default for PlanContext {
    fn default() -> Self {
        Self {
            yes: false,
            needs_consent: Vec::new(),
            jev_degraded: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanCmd {
    AskConsent { items: Vec<ConsentItem> },
    Launch,
    Journal { key: String },
}

impl CmdTrait for PlanCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

fn has_blocked(items: &[ConsentItem]) -> bool {
    items.iter().any(|i| i.reason == ConsentReason::Blocked)
}

pub struct PlanProgramDef;

impl StatechartDefinition for PlanProgramDef {
    type State = PlanState;
    type Event = PlanEvent;
    type Context = PlanContext;
    type Cmd = PlanCmd;
    type Msg = PlanEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(PlanState::Drafting, PlanContext::default())
            .state(PlanState::Drafting, |node| {
                let resolved = node
                    .transition(PlanState::Resolved)
                    .on(|e| matches!(e, PlanEvent::Resolve { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let PlanEvent::Resolve { needs_consent, jev } = e {
                            ctx.needs_consent = needs_consent.clone();
                            ctx.jev_degraded = !matches!(jev, JevOutcome::Ok);
                            if ctx.jev_degraded {
                                q.exec(PlanCmd::Journal {
                                    key: "resolve:jev_degraded".into(),
                                });
                            }
                        }
                    });
                node.on(resolved)
            })
            .state(PlanState::Resolved, |node| {
                let to_awaiting = node
                    .transition(PlanState::AwaitingConsent)
                    .guard(|ctx, _| {
                        !ctx.needs_consent.is_empty()
                            && !(ctx.yes && !has_blocked(&ctx.needs_consent))
                    })
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        q.exec(PlanCmd::AskConsent {
                            items: ctx.needs_consent.clone(),
                        });
                        q.exec(PlanCmd::Journal {
                            key: "resolved:awaiting".into(),
                        });
                    });
                let to_confirmed = node
                    .transition(PlanState::Confirmed)
                    .guard(|ctx, _| {
                        ctx.needs_consent.is_empty()
                            || (ctx.yes && !has_blocked(&ctx.needs_consent))
                    })
                    .action(|_, _, q: &mut ActionQueue<_, _>| {
                        q.exec(PlanCmd::Launch);
                        q.exec(PlanCmd::Journal {
                            key: "resolved:confirmed".into(),
                        });
                    });
                let to_drafting = node
                    .transition(PlanState::Drafting)
                    .on(|e| matches!(e, PlanEvent::Patch { .. }))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.needs_consent.clear();
                        q.exec(PlanCmd::Journal {
                            key: "resolved:patch".into(),
                        });
                    });
                node.always(to_awaiting)
                    .always(to_confirmed)
                    .on(to_drafting)
            })
            .state(PlanState::AwaitingConsent, |node| {
                let confirmed = node
                    .transition(PlanState::Confirmed)
                    .on(|e| matches!(e, PlanEvent::Approve))
                    .guard(|ctx, _| !has_blocked(&ctx.needs_consent))
                    .action(|_, _, q: &mut ActionQueue<_, _>| {
                        q.exec(PlanCmd::Launch);
                        q.exec(PlanCmd::Journal {
                            key: "awaiting:confirmed".into(),
                        });
                    });
                let drafting = node
                    .transition(PlanState::Drafting)
                    .on(|e| matches!(e, PlanEvent::Patch { .. }))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.needs_consent.clear();
                        q.exec(PlanCmd::Journal {
                            key: "awaiting:patch".into(),
                        });
                    });
                let abandoned = node
                    .transition(PlanState::Abandoned)
                    .on(|e| matches!(e, PlanEvent::Abort));
                node.on(confirmed).on(drafting).on(abandoned)
            })
            .final_state(PlanState::Confirmed, |node| node)
            .final_state(PlanState::Abandoned, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        PlanEvent::Abort
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        PlanEvent::Abort
    }
}

#[must_use]
pub fn build_plan_machine() -> Machine<PlanState, PlanEvent, PlanContext, PlanCmd> {
    PlanProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::MachineState;

    fn consent(seat: &str, field: &str, reason: ConsentReason) -> ConsentItem {
        ConsentItem {
            seat: seat.into(),
            field: field.into(),
            reason,
        }
    }

    fn resolve_event(items: Vec<ConsentItem>, jev: JevOutcome) -> PlanEvent {
        PlanEvent::Resolve {
            needs_consent: items,
            jev,
        }
    }

    #[test]
    fn plan_machine_drafting_resolve_empty_goes_confirmed() {
        let m = build_plan_machine();
        let ms = MachineState::at(PlanState::Drafting, PlanContext::default());
        let out = m.step_state(&ms, &resolve_event(vec![], JevOutcome::Ok));
        assert_eq!(out.configuration.primary(), Some(&PlanState::Confirmed));
        assert!(out.commands.iter().any(|c| matches!(c, PlanCmd::Launch)));
    }

    #[test]
    fn plan_machine_drafting_resolve_needs_consent_goes_awaiting() {
        let m = build_plan_machine();
        let ms = MachineState::at(PlanState::Drafting, PlanContext::default());
        let out = m.step_state(
            &ms,
            &resolve_event(vec![consent("s", "f", ConsentReason::Jev)], JevOutcome::Ok),
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&PlanState::AwaitingConsent)
        );
        assert!(out
            .commands
            .iter()
            .any(|c| matches!(c, PlanCmd::AskConsent { .. })));
    }

    #[test]
    fn plan_machine_resolved_patch_goes_drafting() {
        let m = build_plan_machine();
        let mut c = PlanContext::default();
        c.needs_consent = vec![consent("s", "f", ConsentReason::Jev)];
        let ms = MachineState::at(PlanState::Resolved, c);
        let out = m.step_state(
            &ms,
            &PlanEvent::Patch {
                fields: vec!["f".into()],
            },
        );
        assert_eq!(out.configuration.primary(), Some(&PlanState::Drafting));
    }

    #[test]
    fn plan_machine_awaiting_approve_goes_confirmed() {
        let m = build_plan_machine();
        let mut c = PlanContext::default();
        c.needs_consent = vec![consent("s", "f", ConsentReason::Jev)];
        let ms = MachineState::at(PlanState::AwaitingConsent, c);
        let out = m.step_state(&ms, &PlanEvent::Approve);
        assert_eq!(out.configuration.primary(), Some(&PlanState::Confirmed));
    }

    #[test]
    fn plan_machine_awaiting_abort_goes_abandoned() {
        let m = build_plan_machine();
        let ms = MachineState::at(PlanState::AwaitingConsent, PlanContext::default());
        let out = m.step_state(&ms, &PlanEvent::Abort);
        assert_eq!(out.configuration.primary(), Some(&PlanState::Abandoned));
    }

    #[test]
    fn plan_machine_jev_timeout_reaches_resolved_then_awaiting() {
        let m = build_plan_machine();
        let ms = MachineState::at(PlanState::Drafting, PlanContext::default());
        let out = m.step_state(
            &ms,
            &resolve_event(
                vec![consent("s", "f", ConsentReason::Jev)],
                JevOutcome::Timeout,
            ),
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&PlanState::AwaitingConsent)
        );
        assert!(out.context.jev_degraded);
    }

    #[test]
    fn plan_machine_jev_rate_limited_reaches_resolved() {
        let m = build_plan_machine();
        let ms = MachineState::at(PlanState::Drafting, PlanContext::default());
        let out = m.step_state(&ms, &resolve_event(vec![], JevOutcome::RateLimited));
        assert_eq!(out.configuration.primary(), Some(&PlanState::Confirmed));
        assert!(out.context.jev_degraded);
    }

    #[test]
    fn plan_machine_jev_missing_key_reaches_resolved() {
        let m = build_plan_machine();
        let ms = MachineState::at(PlanState::Drafting, PlanContext::default());
        let out = m.step_state(&ms, &resolve_event(vec![], JevOutcome::MissingKey));
        assert_eq!(out.configuration.primary(), Some(&PlanState::Confirmed));
        assert!(out.context.jev_degraded);
    }

    #[test]
    fn plan_machine_jev_shrug_reaches_resolved() {
        let m = build_plan_machine();
        let ms = MachineState::at(PlanState::Drafting, PlanContext::default());
        let out = m.step_state(&ms, &resolve_event(vec![], JevOutcome::Shrug));
        assert_eq!(out.configuration.primary(), Some(&PlanState::Confirmed));
        assert!(out.context.jev_degraded);
    }

    #[test]
    fn plan_machine_blocked_plus_yes_gives_awaiting() {
        let m = build_plan_machine();
        let mut ctx = PlanContext::default();
        ctx.yes = true;
        let ms = MachineState::at(PlanState::Drafting, ctx);
        let out = m.step_state(
            &ms,
            &resolve_event(
                vec![consent("s", "f", ConsentReason::Blocked)],
                JevOutcome::Ok,
            ),
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&PlanState::AwaitingConsent)
        );
    }

    #[test]
    fn plan_machine_blocked_approve_stays_awaiting() {
        let m = build_plan_machine();
        let mut c = PlanContext::default();
        c.needs_consent = vec![consent("s", "f", ConsentReason::Blocked)];
        let ms = MachineState::at(PlanState::AwaitingConsent, c);
        let out = m.step_state(&ms, &PlanEvent::Approve);
        assert_eq!(
            out.configuration.primary(),
            Some(&PlanState::AwaitingConsent)
        );
        assert!(out.commands.is_empty());
    }

    #[test]
    fn plan_machine_blocked_patch_clean_resolve_approve_confirms() {
        let m = build_plan_machine();
        let ms = MachineState::at(PlanState::Drafting, PlanContext::default());
        let out = m.step_state(
            &ms,
            &resolve_event(
                vec![consent("s", "f", ConsentReason::Blocked)],
                JevOutcome::Ok,
            ),
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&PlanState::AwaitingConsent)
        );
        let ms = MachineState::at(
            out.configuration.primary().cloned().unwrap(),
            out.context.clone(),
        );
        let out = m.step_state(
            &ms,
            &PlanEvent::Patch {
                fields: vec!["f".into()],
            },
        );
        assert_eq!(out.configuration.primary(), Some(&PlanState::Drafting));
        let ms = MachineState::at(
            out.configuration.primary().cloned().unwrap(),
            out.context.clone(),
        );
        let out = m.step_state(
            &ms,
            &resolve_event(vec![consent("s", "f", ConsentReason::Jev)], JevOutcome::Ok),
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&PlanState::AwaitingConsent)
        );
        let ms = MachineState::at(
            out.configuration.primary().cloned().unwrap(),
            out.context.clone(),
        );
        let out = m.step_state(&ms, &PlanEvent::Approve);
        assert_eq!(out.configuration.primary(), Some(&PlanState::Confirmed));
    }

    #[test]
    fn plan_machine_empty_consent_goes_confirmed() {
        let m = build_plan_machine();
        let ms = MachineState::at(PlanState::Drafting, PlanContext::default());
        let out = m.step_state(&ms, &resolve_event(vec![], JevOutcome::Ok));
        assert_eq!(out.configuration.primary(), Some(&PlanState::Confirmed));
    }

    #[test]
    fn plan_machine_unregistered_plus_yes_gives_confirmed() {
        let m = build_plan_machine();
        let mut ctx = PlanContext::default();
        ctx.yes = true;
        let ms = MachineState::at(PlanState::Drafting, ctx);
        let out = m.step_state(
            &ms,
            &resolve_event(
                vec![consent("s", "f", ConsentReason::Unregistered)],
                JevOutcome::Ok,
            ),
        );
        assert_eq!(out.configuration.primary(), Some(&PlanState::Confirmed));
    }

    #[test]
    fn plan_machine_terminal_states_emit_no_cmds() {
        let m = build_plan_machine();
        for state in [PlanState::Confirmed, PlanState::Abandoned] {
            let ms = MachineState::at(state, PlanContext::default());
            for event in [
                resolve_event(vec![], JevOutcome::Ok),
                PlanEvent::Patch { fields: vec![] },
                PlanEvent::Approve,
                PlanEvent::Abort,
            ] {
                let out = m.step_state(&ms, &event);
                assert!(
                    out.commands.is_empty(),
                    "{state:?} + {event:?} must emit nothing"
                );
                assert_eq!(
                    out.configuration.primary(),
                    Some(&state),
                    "{state:?} terminal stays"
                );
            }
        }
    }

    #[test]
    fn plan_machine_message_log_replay_is_deterministic() {
        let m = build_plan_machine();
        let log = vec![
            resolve_event(vec![consent("s", "f", ConsentReason::Jev)], JevOutcome::Ok),
            PlanEvent::Approve,
        ];
        let run = |events: &[PlanEvent]| {
            let mut ms = MachineState::at(PlanState::Drafting, PlanContext::default());
            let mut cmds = Vec::new();
            for e in events {
                let out = m.step_state(&ms, e);
                cmds.extend(out.commands.clone());
                ms = MachineState::at(
                    out.configuration.primary().cloned().unwrap(),
                    out.context.clone(),
                );
            }
            (ms, cmds)
        };
        assert_eq!(run(&log), run(&log));
    }
}
