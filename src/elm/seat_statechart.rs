//! Seat lifecycle statechart (ADR-0022 §3.2).
//!
//! One seat from launch to retirement. Pure fold; commands are descriptive
//! only. Closed `SeatFailureKind` duplicates [`crate::failure::FailureKind`]
//! vocabulary; unify in bd-gb4at.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

use crate::seat_reconcile::SeatEffect;

/// Closed failure taxonomy for V6 (toron ADR-0032 kinds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SeatFailureKind {
    Auth,
    RateLimit,
    Timeout,
    Blocked,
    Provider,
    Cancelled,
    Unknown,
    Billing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SeatState {
    Launching,
    Live,
    Failing,
    Relaunching,
    Respawning,
    Morphing,
    Retired,
    Dead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeatEvent {
    Ready,
    LaunchFailed {
        kind: SeatFailureKind,
        evidence: String,
    },
    Failed {
        kind: SeatFailureKind,
        evidence: String,
    },
    Patch(SeatEffect),
    Retire,
    IdleTimeout,
    Ack,
    OldPaneClosed,
    RelaunchFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatContext {
    pub pin: String,
    pub hops_spent: BTreeSet<SeatFailureKind>,
    pub model_by_env: bool,
    pub last_failure: Option<(SeatFailureKind, String)>,
}

impl Default for SeatContext {
    fn default() -> Self {
        Self {
            pin: String::new(),
            hops_spent: BTreeSet::new(),
            model_by_env: false,
            last_failure: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeatCmd {
    Relaunch { pin: String },
    Respawn { pin: String },
    InjectRole { pin: String, role: String },
    ClosePane { pin: String },
    RetireAfterTurn { pin: String },
    Journal { key: String },
}

impl CmdTrait for SeatCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

fn is_timeout_or_unknown(kind: SeatFailureKind) -> bool {
    matches!(kind, SeatFailureKind::Timeout | SeatFailureKind::Unknown)
}

fn hop_allowed(ctx: &SeatContext, kind: SeatFailureKind, evidence: &str) -> bool {
    !is_timeout_or_unknown(kind) && !evidence.trim().is_empty() && !ctx.hops_spent.contains(&kind)
}

fn patch_is_model_relaunch(event: &SeatEvent) -> bool {
    match event {
        SeatEvent::Patch(SeatEffect::Relaunch { fields, .. }) => {
            fields.iter().any(|f| f == "model")
        }
        _ => false,
    }
}

pub struct SeatProgramDef;

impl StatechartDefinition for SeatProgramDef {
    type State = SeatState;
    type Event = SeatEvent;
    type Context = SeatContext;
    type Cmd = SeatCmd;
    type Msg = SeatEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(SeatState::Launching, SeatContext::default())
            .state(SeatState::Launching, |node| {
                let live = node
                    .transition(SeatState::Live)
                    .on(|e| matches!(e, SeatEvent::Ready))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        q.exec(SeatCmd::Journal {
                            key: format!("{}:ready", ctx.pin),
                        });
                    });
                let failing = node
                    .transition(SeatState::Failing)
                    .on(|e| matches!(e, SeatEvent::LaunchFailed { .. }))
                    .action(|ctx, e, _q: &mut ActionQueue<_, _>| {
                        if let SeatEvent::LaunchFailed { kind, evidence } = e {
                            ctx.last_failure = Some((*kind, evidence.clone()));
                        }
                    });
                node.on(live).on(failing)
            })
            .state(SeatState::Live, |node| {
                let failing = node
                    .transition(SeatState::Failing)
                    .on(|e| matches!(e, SeatEvent::Failed { .. }))
                    .action(|ctx, e, _q: &mut ActionQueue<_, _>| {
                        if let SeatEvent::Failed { kind, evidence } = e {
                            ctx.last_failure = Some((*kind, evidence.clone()));
                        }
                    });
                let relaunch = node
                    .transition(SeatState::Relaunching)
                    .on(|e| matches!(e, SeatEvent::Patch(SeatEffect::Relaunch { .. })))
                    .guard(|ctx, e| !(ctx.model_by_env && patch_is_model_relaunch(e)))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let SeatEvent::Patch(SeatEffect::Relaunch { .. }) = e {
                            q.exec(SeatCmd::Relaunch {
                                pin: ctx.pin.clone(),
                            });
                            q.exec(SeatCmd::Journal {
                                key: format!("{}:relaunch", ctx.pin),
                            });
                        }
                    });
                let respawn_model = node
                    .transition(SeatState::Respawning)
                    .on(|e| matches!(e, SeatEvent::Patch(SeatEffect::Relaunch { .. })))
                    .guard(|ctx, e| ctx.model_by_env && patch_is_model_relaunch(e))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        q.exec(SeatCmd::Respawn {
                            pin: ctx.pin.clone(),
                        });
                        q.exec(SeatCmd::Journal {
                            key: format!("{}:respawn", ctx.pin),
                        });
                    });
                let respawn = node
                    .transition(SeatState::Respawning)
                    .on(|e| matches!(e, SeatEvent::Patch(SeatEffect::Respawn { .. })))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        q.exec(SeatCmd::Respawn {
                            pin: ctx.pin.clone(),
                        });
                        q.exec(SeatCmd::Journal {
                            key: format!("{}:respawn", ctx.pin),
                        });
                    });
                let morph = node
                    .transition(SeatState::Morphing)
                    .on(|e| matches!(e, SeatEvent::Patch(SeatEffect::InjectRole { .. })))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let SeatEvent::Patch(SeatEffect::InjectRole { role, .. }) = e {
                            q.exec(SeatCmd::InjectRole {
                                pin: ctx.pin.clone(),
                                role: role.clone(),
                            });
                            q.exec(SeatCmd::Journal {
                                key: format!("{}:inject", ctx.pin),
                            });
                        }
                    });
                let retire = node
                    .transition(SeatState::Retired)
                    .on(|e| matches!(e, SeatEvent::Retire))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        q.exec(SeatCmd::ClosePane {
                            pin: ctx.pin.clone(),
                        });
                    });
                let idle = node
                    .transition(SeatState::Retired)
                    .on(|e| matches!(e, SeatEvent::IdleTimeout))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        q.exec(SeatCmd::RetireAfterTurn {
                            pin: ctx.pin.clone(),
                        });
                    });
                node.on(failing)
                    .on(relaunch)
                    .on(respawn_model)
                    .on(respawn)
                    .on(morph)
                    .on(retire)
                    .on(idle)
            })
            .state(SeatState::Failing, |node| {
                let relaunch = node
                    .transition(SeatState::Relaunching)
                    .guard(|ctx, _| match &ctx.last_failure {
                        Some((kind, evidence)) => {
                            hop_allowed(ctx, *kind, evidence) && !ctx.model_by_env
                        }
                        None => false,
                    })
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        if let Some((kind, _)) = ctx.last_failure.clone() {
                            ctx.hops_spent.insert(kind);
                        }
                        ctx.last_failure = None;
                        q.exec(SeatCmd::Relaunch {
                            pin: ctx.pin.clone(),
                        });
                        q.exec(SeatCmd::Journal {
                            key: format!("{}:relaunch", ctx.pin),
                        });
                    });
                let respawn = node
                    .transition(SeatState::Respawning)
                    .guard(|ctx, _| match &ctx.last_failure {
                        Some((kind, evidence)) => {
                            hop_allowed(ctx, *kind, evidence) && ctx.model_by_env
                        }
                        None => false,
                    })
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        if let Some((kind, _)) = ctx.last_failure.clone() {
                            ctx.hops_spent.insert(kind);
                        }
                        ctx.last_failure = None;
                        q.exec(SeatCmd::Respawn {
                            pin: ctx.pin.clone(),
                        });
                        q.exec(SeatCmd::Journal {
                            key: format!("{}:respawn", ctx.pin),
                        });
                    });
                let dead =
                    node.transition(SeatState::Dead)
                        .guard(|ctx, _| match &ctx.last_failure {
                            Some((kind, evidence)) => !hop_allowed(ctx, *kind, evidence),
                            None => true,
                        });
                node.always(relaunch).always(respawn).always(dead)
            })
            .state(SeatState::Relaunching, |node| {
                let live = node
                    .transition(SeatState::Live)
                    .on(|e| matches!(e, SeatEvent::Ready));
                let dead = node
                    .transition(SeatState::Dead)
                    .on(|e| matches!(e, SeatEvent::RelaunchFailed));
                node.on(live).on(dead)
            })
            .state(SeatState::Respawning, |node| {
                let launching = node
                    .transition(SeatState::Launching)
                    .on(|e| matches!(e, SeatEvent::OldPaneClosed));
                node.on(launching)
            })
            .state(SeatState::Morphing, |node| {
                let live = node
                    .transition(SeatState::Live)
                    .on(|e| matches!(e, SeatEvent::Ack));
                node.on(live)
            })
            .final_state(SeatState::Retired, |node| node)
            .final_state(SeatState::Dead, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        SeatEvent::Ready
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        SeatEvent::RelaunchFailed
    }
}

#[must_use]
pub fn build_seat_machine() -> Machine<SeatState, SeatEvent, SeatContext, SeatCmd> {
    SeatProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::{Boot, MachineState, Program};

    fn ctx(pin: &str, env: bool) -> SeatContext {
        SeatContext {
            pin: pin.into(),
            hops_spent: BTreeSet::new(),
            model_by_env: env,
            last_failure: None,
        }
    }

    fn failed(kind: SeatFailureKind) -> SeatEvent {
        SeatEvent::Failed {
            kind,
            evidence: "boom".into(),
        }
    }

    #[test]
    fn seat_machine_launching_ready_goes_live() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Launching, ctx("a", false));
        let out = m.step_state(&ms, &SeatEvent::Ready);
        assert_eq!(out.configuration.primary(), Some(&SeatState::Live));
    }

    #[test]
    fn seat_machine_launching_launch_failed_goes_failing() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Launching, ctx("a", false));
        let out = m.step_state(
            &ms,
            &SeatEvent::LaunchFailed {
                kind: SeatFailureKind::Provider,
                evidence: "x".into(),
            },
        );
        assert_eq!(out.configuration.primary(), Some(&SeatState::Relaunching));
    }

    #[test]
    fn seat_machine_live_failed_goes_failing() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", false));
        let out = m.step_state(&ms, &failed(SeatFailureKind::Provider));
        assert_eq!(out.configuration.primary(), Some(&SeatState::Relaunching));
    }

    #[test]
    fn seat_machine_live_patch_argv_relaunch_goes_relaunching() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", false));
        let out = m.step_state(
            &ms,
            &SeatEvent::Patch(SeatEffect::Relaunch {
                pin: "a".into(),
                fields: vec!["effort".into()],
            }),
        );
        assert_eq!(out.configuration.primary(), Some(&SeatState::Relaunching));
        assert!(out.commands.iter().any(|c| matches!(
            c,
            SeatCmd::Relaunch { pin } if pin == "a"
        )));
    }

    #[test]
    fn seat_machine_live_patch_env_model_never_relaunches() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", true));
        let out = m.step_state(
            &ms,
            &SeatEvent::Patch(SeatEffect::Relaunch {
                pin: "a".into(),
                fields: vec!["model".into()],
            }),
        );
        assert_eq!(out.configuration.primary(), Some(&SeatState::Respawning));
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, SeatCmd::Relaunch { .. }))
        );
        assert!(out.commands.iter().any(|c| matches!(
            c,
            SeatCmd::Respawn { pin } if pin == "a"
        )));
    }

    #[test]
    fn seat_machine_live_patch_env_model_effort_still_respawns() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", true));
        let out = m.step_state(
            &ms,
            &SeatEvent::Patch(SeatEffect::Relaunch {
                pin: "a".into(),
                fields: vec!["model".into(), "effort".into()],
            }),
        );
        assert_eq!(out.configuration.primary(), Some(&SeatState::Respawning));
    }

    #[test]
    fn seat_machine_live_patch_respawn_goes_respawning() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", false));
        let out = m.step_state(
            &ms,
            &SeatEvent::Patch(SeatEffect::Respawn {
                pin: "a".into(),
                fields: vec!["harness".into()],
                context_fresh: true,
            }),
        );
        assert_eq!(out.configuration.primary(), Some(&SeatState::Respawning));
    }

    #[test]
    fn seat_machine_live_patch_inject_goes_morphing() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", false));
        let out = m.step_state(
            &ms,
            &SeatEvent::Patch(SeatEffect::InjectRole {
                pin: "a".into(),
                role: "reviewer".into(),
            }),
        );
        assert_eq!(out.configuration.primary(), Some(&SeatState::Morphing));
        assert!(out.commands.iter().any(|c| matches!(
            c,
            SeatCmd::InjectRole { pin, .. } if pin == "a"
        )));
    }

    #[test]
    fn seat_machine_live_retire_goes_retired() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", false));
        let out = m.step_state(&ms, &SeatEvent::Retire);
        assert_eq!(out.configuration.primary(), Some(&SeatState::Retired));
        assert!(out.commands.iter().any(|c| matches!(
            c,
            SeatCmd::ClosePane { pin } if pin == "a"
        )));
    }

    #[test]
    fn seat_machine_live_idle_timeout_goes_retired() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", false));
        let out = m.step_state(&ms, &SeatEvent::IdleTimeout);
        assert_eq!(out.configuration.primary(), Some(&SeatState::Retired));
        assert!(out.commands.iter().any(|c| matches!(
            c,
            SeatCmd::RetireAfterTurn { pin } if pin == "a"
        )));
    }

    #[test]
    fn seat_machine_failing_timeout_goes_dead() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", false));
        let out = m.step_state(
            &ms,
            &SeatEvent::Failed {
                kind: SeatFailureKind::Timeout,
                evidence: "t".into(),
            },
        );
        assert_eq!(out.configuration.primary(), Some(&SeatState::Dead));
        assert!(out.commands.is_empty());
    }

    #[test]
    fn seat_machine_failing_unknown_goes_dead() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", false));
        let out = m.step_state(
            &ms,
            &SeatEvent::Failed {
                kind: SeatFailureKind::Unknown,
                evidence: "u".into(),
            },
        );
        assert_eq!(out.configuration.primary(), Some(&SeatState::Dead));
    }

    #[test]
    fn seat_machine_failing_empty_evidence_goes_dead() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Launching, ctx("a", false));
        let out = m.step_state(
            &ms,
            &SeatEvent::LaunchFailed {
                kind: SeatFailureKind::Provider,
                evidence: "  ".into(),
            },
        );
        assert_eq!(out.configuration.primary(), Some(&SeatState::Dead));
    }

    #[test]
    fn seat_machine_failing_second_same_class_goes_dead() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", false));
        let out = m.step_state(&ms, &failed(SeatFailureKind::Provider));
        assert_eq!(out.configuration.primary(), Some(&SeatState::Relaunching));
        let ms = MachineState::at(SeatState::Relaunching, out.context);
        let out = m.step_state(&ms, &SeatEvent::Ready);
        assert_eq!(out.configuration.primary(), Some(&SeatState::Live));
        let ms = MachineState::at(SeatState::Live, out.context);
        let out = m.step_state(&ms, &failed(SeatFailureKind::Provider));
        assert_eq!(out.configuration.primary(), Some(&SeatState::Dead));
    }

    #[test]
    fn seat_machine_failing_different_class_may_hop_once() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Live, ctx("a", false));
        let out = m.step_state(&ms, &failed(SeatFailureKind::RateLimit));
        assert_eq!(out.configuration.primary(), Some(&SeatState::Relaunching));
        let ms = MachineState::at(SeatState::Relaunching, out.context);
        let out = m.step_state(&ms, &SeatEvent::Ready);
        assert_eq!(out.configuration.primary(), Some(&SeatState::Live));
        let ms = MachineState::at(SeatState::Live, out.context);
        let out = m.step_state(&ms, &failed(SeatFailureKind::Provider));
        assert_eq!(out.configuration.primary(), Some(&SeatState::Relaunching));
        assert!(out.context.hops_spent.contains(&SeatFailureKind::Provider));
    }

    #[test]
    fn seat_machine_relaunching_ready_goes_live() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Relaunching, ctx("a", false));
        let out = m.step_state(&ms, &SeatEvent::Ready);
        assert_eq!(out.configuration.primary(), Some(&SeatState::Live));
    }

    #[test]
    fn seat_machine_respawning_old_pane_closed_keeps_pin() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Respawning, ctx("a", true));
        let out = m.step_state(&ms, &SeatEvent::OldPaneClosed);
        assert_eq!(out.configuration.primary(), Some(&SeatState::Launching));
        assert_eq!(out.context.pin.as_str(), "a");
    }

    #[test]
    fn seat_machine_morphing_ack_goes_live() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Morphing, ctx("a", false));
        let out = m.step_state(&ms, &SeatEvent::Ack);
        assert_eq!(out.configuration.primary(), Some(&SeatState::Live));
    }

    #[test]
    fn seat_machine_relaunching_failed_goes_dead() {
        let m = build_seat_machine();
        let ms = MachineState::at(SeatState::Relaunching, ctx("a", false));
        let out = m.step_state(&ms, &SeatEvent::RelaunchFailed);
        assert_eq!(out.configuration.primary(), Some(&SeatState::Dead));
    }

    #[test]
    fn seat_machine_terminal_states_emit_no_cmds() {
        let m = build_seat_machine();
        for state in [SeatState::Retired, SeatState::Dead] {
            let ms = MachineState::at(state, ctx("a", false));
            for event in [
                SeatEvent::Ready,
                failed(SeatFailureKind::Provider),
                SeatEvent::Retire,
                SeatEvent::IdleTimeout,
                SeatEvent::Ack,
                SeatEvent::OldPaneClosed,
                SeatEvent::RelaunchFailed,
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
    fn seat_machine_message_log_replay_is_deterministic() {
        let m = build_seat_machine();
        let log = vec![
            SeatEvent::Ready,
            SeatEvent::Patch(SeatEffect::Relaunch {
                pin: "a".into(),
                fields: vec!["effort".into()],
            }),
            SeatEvent::Ready,
        ];
        let run = |events: &[SeatEvent]| {
            let mut ms = MachineState::at(SeatState::Launching, ctx("a", false));
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
