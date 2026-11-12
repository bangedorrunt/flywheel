//! Unified swarm lifecycle statechart (ADR-0020 Phase 4).
//!
//! Models the multi-agent swarm wave as a pure `elm-effect-xstate` machine:
//! classify, trap mitigation, dispatch, execute, review, and verify, with P2P
//! directive and Chie memory trap events folded into the same transition
//! surface. The fold is pure; commands are descriptions only.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use toron_core::kernel::{
    ActionQueue, Cmd as CmdTrait, Durability, Failure, Machine, StatechartDefinition,
    StatechartProgram,
};

/// Swarm lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SwarmState {
    Classify,
    MitigatingTrap,
    Dispatch,
    Execute,
    Review,
    Verify,
    Complete,
    Failed,
}

/// Domain events driving the swarm lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwarmEvent {
    Init,
    Classify {
        target: String,
    },
    Classified {
        mode: String,
    },
    DispatchBead {
        bead_id: String,
        assignee: String,
    },
    ClaimSeen {
        bead_id: String,
        agent: String,
    },
    StageDone {
        stage: String,
    },
    P2PDirective {
        verb: String,
        sender: String,
        recipient: String,
        path: String,
        bead: Option<String>,
    },
    TrapDetected {
        query: String,
        trap_summary: String,
    },
    ReviewVerdict {
        passed: bool,
        severity: u8,
        notes: String,
    },
    VerifyVerdict {
        passed: bool,
        output: String,
    },
    TimerFired {
        tag: String,
    },
}

/// Quantitative swarm context carried by the statechart.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SwarmContext {
    pub target: Option<String>,
    pub active_bead: Option<String>,
    pub assignee: Option<String>,
    pub held_paths: Vec<String>,
    pub review_verdict: Option<(bool, u8, String)>,
    pub verify_verdict: Option<(bool, String)>,
    pub traps_detected: Vec<String>,
    pub p2p_directives_log: Vec<String>,
    pub stage_history: Vec<String>,
}

/// Commands emitted by the swarm statechart. Ephemeral: the fold describes
/// intent, the host interprets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwarmCmd {
    AcquireLease {
        path: String,
    },
    ReleaseLease {
        path: String,
    },
    WritePaneWake {
        target: String,
        line: String,
    },
    EvaluateJudgment {
        verb: String,
        sender: String,
        recipient: String,
        path: String,
        bead: Option<String>,
    },
    CheckMemoryTrap {
        query: String,
    },
    RecordMemoryLesson {
        lesson: String,
    },
    DispatchPrompt {
        assignee: String,
        bead_id: String,
    },
}

impl CmdTrait for SwarmCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

/// Statechart definition for the swarm lifecycle.
pub struct SwarmProgramDef;

fn is_p2p(event: &SwarmEvent) -> bool {
    matches!(event, SwarmEvent::P2PDirective { .. })
}

fn is_p2p_non_review(event: &SwarmEvent) -> bool {
    matches!(event, SwarmEvent::P2PDirective { verb, .. } if verb != "review-request")
}

fn p2p_verb(event: &SwarmEvent, verb: &str) -> bool {
    matches!(event, SwarmEvent::P2PDirective { verb: v, .. } if v == verb)
}

fn stage_done(event: &SwarmEvent, stage: &str) -> bool {
    matches!(event, SwarmEvent::StageDone { stage: s } if s == stage)
}

fn record_stage(ctx: &mut SwarmContext, event: &SwarmEvent) {
    match event {
        SwarmEvent::Classified { mode } => ctx.stage_history.push(format!("classified:{mode}")),
        SwarmEvent::DispatchBead { bead_id, assignee } => {
            ctx.active_bead = Some(bead_id.clone());
            ctx.assignee = Some(assignee.clone());
            ctx.stage_history.push(format!("dispatch:{bead_id}"));
        }
        SwarmEvent::ClaimSeen { bead_id, agent } => {
            ctx.active_bead = Some(bead_id.clone());
            ctx.assignee = Some(agent.clone());
            ctx.stage_history.push(format!("claim:{bead_id}"));
        }
        SwarmEvent::StageDone { stage } => ctx.stage_history.push(format!("stage:{stage}")),
        SwarmEvent::P2PDirective {
            verb,
            sender,
            recipient,
            path,
            bead,
        } => {
            ctx.p2p_directives_log.push(format!(
                "{verb}:{sender}->{recipient}:{path}{}",
                bead.as_ref().map(|b| format!(":{b}")).unwrap_or_default()
            ));
            if verb == "review-request" {
                ctx.stage_history.push("p2p:review-request".into());
            }
        }
        SwarmEvent::TrapDetected { query, .. } => {
            ctx.traps_detected.push(query.clone());
            ctx.stage_history.push("trap-detected".into());
        }
        SwarmEvent::ReviewVerdict {
            passed,
            severity,
            notes,
        } => {
            ctx.review_verdict = Some((*passed, *severity, notes.clone()));
            ctx.stage_history
                .push(format!("review:{}", if *passed { "pass" } else { "fail" }));
        }
        SwarmEvent::VerifyVerdict { passed, output } => {
            ctx.verify_verdict = Some((*passed, output.clone()));
            ctx.stage_history
                .push(format!("verify:{}", if *passed { "pass" } else { "fail" }));
        }
        SwarmEvent::Classify { target } => {
            ctx.target = Some(target.clone());
            ctx.stage_history.push("classify".into());
        }
        SwarmEvent::Init | SwarmEvent::TimerFired { .. } => {}
    }
}

fn emit_p2p_judgment(
    ctx: &mut SwarmContext,
    event: &SwarmEvent,
    q: &mut ActionQueue<SwarmCmd, SwarmEvent>,
) {
    record_stage(ctx, event);
    if let SwarmEvent::P2PDirective {
        verb,
        sender,
        recipient,
        path,
        bead,
    } = event
    {
        q.exec(SwarmCmd::EvaluateJudgment {
            verb: verb.clone(),
            sender: sender.clone(),
            recipient: recipient.clone(),
            path: path.clone(),
            bead: bead.clone(),
        });
    }
}

impl StatechartDefinition for SwarmProgramDef {
    type State = SwarmState;
    type Event = SwarmEvent;
    type Context = SwarmContext;
    type Cmd = SwarmCmd;
    type Msg = SwarmEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(SwarmState::Classify, SwarmContext::default())
            .state(SwarmState::Classify, |node| {
                let classified = node
                    .transition(SwarmState::Dispatch)
                    .on(|e| matches!(e, SwarmEvent::Classified { .. }))
                    .assign(record_stage);
                let trap = node
                    .transition(SwarmState::MitigatingTrap)
                    .on(|e| matches!(e, SwarmEvent::TrapDetected { .. }))
                    .assign(record_stage);
                let init = node
                    .transition(SwarmState::Classify)
                    .on(|e| matches!(e, SwarmEvent::Classify { .. }))
                    .assign(record_stage);
                node.on(classified).on(trap).on(init)
            })
            .state(SwarmState::MitigatingTrap, |node| {
                let mitigated = node
                    .transition(SwarmState::Dispatch)
                    .on(|e| matches!(e, SwarmEvent::Classified { .. }))
                    .assign(record_stage);
                node.on(mitigated)
            })
            .state(SwarmState::Dispatch, |node| {
                let p2p = node.targetless().on(is_p2p).action(emit_p2p_judgment);
                let claimed = node
                    .transition(SwarmState::Execute)
                    .on(|e| matches!(e, SwarmEvent::ClaimSeen { .. }))
                    .assign(record_stage);
                let staged = node
                    .transition(SwarmState::Execute)
                    .on(|e| stage_done(e, "dispatch"))
                    .assign(record_stage);
                let dispatched = node
                    .transition(SwarmState::Dispatch)
                    .on(|e| matches!(e, SwarmEvent::DispatchBead { .. }))
                    .action(|ctx, e, q| {
                        record_stage(ctx, e);
                        if let SwarmEvent::DispatchBead { bead_id, assignee } = e {
                            q.exec(SwarmCmd::DispatchPrompt {
                                assignee: assignee.clone(),
                                bead_id: bead_id.clone(),
                            });
                        }
                    });
                node.on(p2p).on(claimed).on(staged).on(dispatched)
            })
            .state(SwarmState::Execute, |node| {
                let p2p_review = node
                    .transition(SwarmState::Review)
                    .on(|e| p2p_verb(e, "review-request"))
                    .action(emit_p2p_judgment);
                let p2p = node
                    .targetless()
                    .on(is_p2p_non_review)
                    .action(emit_p2p_judgment);
                let review = node
                    .transition(SwarmState::Review)
                    .on(|e| stage_done(e, "execute"))
                    .assign(record_stage);
                node.on(p2p_review).on(p2p).on(review)
            })
            .state(SwarmState::Review, |node| {
                let to_verify = node
                    .transition(SwarmState::Verify)
                    .on(|e| matches!(e, SwarmEvent::ReviewVerdict { passed: true, .. }))
                    .assign(record_stage);
                let to_verify_stage = node
                    .transition(SwarmState::Verify)
                    .on(|e| stage_done(e, "review"))
                    .assign(record_stage);
                let back = node
                    .transition(SwarmState::Execute)
                    .on(|e| matches!(e, SwarmEvent::ReviewVerdict { passed: false, .. }))
                    .assign(record_stage);
                node.on(to_verify).on(to_verify_stage).on(back)
            })
            .state(SwarmState::Verify, |node| {
                let ok = node
                    .transition(SwarmState::Complete)
                    .on(|e| matches!(e, SwarmEvent::VerifyVerdict { passed: true, .. }))
                    .assign(record_stage);
                let back = node
                    .transition(SwarmState::Execute)
                    .on(|e| matches!(e, SwarmEvent::VerifyVerdict { passed: false, .. }))
                    .assign(record_stage);
                node.on(ok).on(back)
            })
            .state(SwarmState::Complete, |node| node)
            .state(SwarmState::Failed, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        SwarmEvent::Init
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &Failure<Self::Err>) -> Self::Msg {
        SwarmEvent::TimerFired {
            tag: "failure".into(),
        }
    }
}

/// Build the swarm lifecycle machine.
#[must_use]
pub fn build_swarm_machine() -> Machine<SwarmState, SwarmEvent, SwarmContext, SwarmCmd> {
    SwarmProgramDef::machine()
}

/// Elm program adapter for the swarm statechart.
pub type SwarmProgram = StatechartProgram<SwarmProgramDef>;

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::{Boot, Program, StatechartModel, Update};

    fn step(
        model: &StatechartModel<SwarmState, SwarmContext>,
        msg: &SwarmEvent,
    ) -> Update<StatechartModel<SwarmState, SwarmContext>, SwarmCmd> {
        SwarmProgram::update(model, msg)
    }

    #[test]
    fn full_wave_happy_path() {
        let init = SwarmProgram::init(&Boot::default());
        assert_eq!(init.model.state(), Some(&SwarmState::Classify));

        let m = step(
            &init.model,
            &SwarmEvent::Classified {
                mode: "swarm".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Dispatch));

        let m = step(
            &m.model,
            &SwarmEvent::ClaimSeen {
                bead_id: "bd-x".into(),
                agent: "BlackGorge".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Execute));

        let m = step(
            &m.model,
            &SwarmEvent::StageDone {
                stage: "execute".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Review));

        let m = step(
            &m.model,
            &SwarmEvent::ReviewVerdict {
                passed: true,
                severity: 1,
                notes: "ok".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Verify));

        let m = step(
            &m.model,
            &SwarmEvent::VerifyVerdict {
                passed: true,
                output: "green".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Complete));
        assert_eq!(
            m.model.context.verify_verdict.as_ref().map(|v| v.0),
            Some(true)
        );
    }

    #[test]
    fn review_rejection_loops_back_to_execute() {
        let init = SwarmProgram::init(&Boot::default());
        let m = step(
            &init.model,
            &SwarmEvent::Classified {
                mode: "swarm".into(),
            },
        );
        let m = step(
            &m.model,
            &SwarmEvent::ClaimSeen {
                bead_id: "bd-x".into(),
                agent: "BlackGorge".into(),
            },
        );
        let m = step(
            &m.model,
            &SwarmEvent::StageDone {
                stage: "execute".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Review));

        let m = step(
            &m.model,
            &SwarmEvent::ReviewVerdict {
                passed: false,
                severity: 3,
                notes: "fix".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Execute));

        let m = step(
            &m.model,
            &SwarmEvent::StageDone {
                stage: "execute".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Review));
    }

    #[test]
    fn p2p_directive_emits_advisory_judgment_command() {
        let init = SwarmProgram::init(&Boot::default());
        let m = step(
            &init.model,
            &SwarmEvent::Classified {
                mode: "swarm".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Dispatch));

        let p2p = SwarmEvent::P2PDirective {
            verb: "intent".into(),
            sender: "A".into(),
            recipient: "B".into(),
            path: "src/lib.rs".into(),
            bead: Some("bd-1".into()),
        };
        let m = step(&m.model, &p2p);
        assert_eq!(m.model.state(), Some(&SwarmState::Dispatch));
        assert!(
            m.commands
                .iter()
                .any(|c| matches!(c, SwarmCmd::EvaluateJudgment { verb, .. } if verb == "intent"))
        );
        assert!(!m.model.context.p2p_directives_log.is_empty());

        let m = step(
            &m.model,
            &SwarmEvent::ClaimSeen {
                bead_id: "bd-1".into(),
                agent: "A".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Execute));
        let m = step(
            &m.model,
            &SwarmEvent::P2PDirective {
                verb: "intent".into(),
                sender: "B".into(),
                recipient: "A".into(),
                path: "src/lib.rs".into(),
                bead: None,
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Execute));
        assert!(
            m.commands
                .iter()
                .any(|c| matches!(c, SwarmCmd::EvaluateJudgment { .. }))
        );
    }

    #[test]
    fn chie_trap_detection_transition() {
        let init = SwarmProgram::init(&Boot::default());
        let trap = SwarmEvent::TrapDetected {
            query: "rm -rf".into(),
            trap_summary: "dangerous".into(),
        };
        let m = step(&init.model, &trap);
        assert_eq!(m.model.state(), Some(&SwarmState::MitigatingTrap));
        assert_eq!(m.model.context.traps_detected, vec!["rm -rf".to_string()]);

        let m = step(
            &m.model,
            &SwarmEvent::Classified {
                mode: "safe".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Dispatch));
    }

    #[test]
    fn tla_export_named_swarm_lifecycle() {
        let machine = build_swarm_machine();
        let tla = machine.to_tla_named("SwarmLifecycle");
        assert!(tla.starts_with("---- MODULE SwarmLifecycle ----\n"));
        assert!(tla.contains("VARIABLES state"));
        assert!(tla.contains("States == {"));
        assert!(tla.contains("\"Classify\""));
        assert!(tla.contains("\"Dispatch\""));
        assert!(tla.contains("\"Execute\""));
        assert!(tla.contains("\"Review\""));
        assert!(tla.contains("\"Verify\""));
        assert!(tla.contains("\"Complete\""));
        assert!(tla.contains("Init == state = \"Classify\""));
        assert!(tla.ends_with("====\n"));
    }

    #[test]
    fn statechart_program_elm_loop_step() {
        let init = SwarmProgram::init(&Boot::default());
        assert_eq!(init.model.state(), Some(&SwarmState::Classify));
        assert!(init.commands.is_empty());

        let u = SwarmProgram::update(
            &init.model,
            &SwarmEvent::Classified {
                mode: "swarm".into(),
            },
        );
        assert_eq!(u.model.state(), Some(&SwarmState::Dispatch));
        assert!(u.commands.is_empty());

        let u2 = SwarmProgram::update(
            &u.model,
            &SwarmEvent::ClaimSeen {
                bead_id: "bd-9".into(),
                agent: "BlackGorge".into(),
            },
        );
        assert_eq!(u2.model.state(), Some(&SwarmState::Execute));

        let fail_msg = SwarmProgram::failure(&Failure::Fail("boom".into()));
        assert!(matches!(fail_msg, SwarmEvent::TimerFired { .. }));
    }

    #[test]
    fn verify_failure_loops_back_to_execute() {
        let init = SwarmProgram::init(&Boot::default());
        let m = step(
            &init.model,
            &SwarmEvent::Classified {
                mode: "swarm".into(),
            },
        );
        let m = step(
            &m.model,
            &SwarmEvent::ClaimSeen {
                bead_id: "bd-x".into(),
                agent: "BlackGorge".into(),
            },
        );
        let m = step(
            &m.model,
            &SwarmEvent::StageDone {
                stage: "execute".into(),
            },
        );
        let m = step(
            &m.model,
            &SwarmEvent::ReviewVerdict {
                passed: true,
                severity: 0,
                notes: "ok".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Verify));

        let m = step(
            &m.model,
            &SwarmEvent::VerifyVerdict {
                passed: false,
                output: "red".into(),
            },
        );
        assert_eq!(m.model.state(), Some(&SwarmState::Execute));
    }
}
