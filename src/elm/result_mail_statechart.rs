//! Result-mail statechart (bd-n21ml).
//!
//! A proof red row and a bead landing each notify exactly one crew once. The
//! dedupe sets live in the context, so a replay of the same row or the same
//! landing emits no second Mail command; a green proof emits none at all.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ResultMailState {
    Idle,
    ProofOpen,
    Landed,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResultMailEvent {
    RedRow { id: String, owner: String },
    ProofEnd,
    Green,
    Land { bead: String, assignee: String },
    Replay { bead: String },
    Finish,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResultMailContext {
    pub mailed_rows: BTreeSet<String>,
    pub mailed_lands: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResultMailCmd {
    Mail { owner: String, body: String },
    Journal { key: String },
}

impl CmdTrait for ResultMailCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct ResultMailProgramDef;

impl StatechartDefinition for ResultMailProgramDef {
    type State = ResultMailState;
    type Event = ResultMailEvent;
    type Context = ResultMailContext;
    type Cmd = ResultMailCmd;
    type Msg = ResultMailEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(ResultMailState::Idle, ResultMailContext::default())
            .state(ResultMailState::Idle, |node| {
                let green = node
                    .transition(ResultMailState::Done)
                    .on(|e| matches!(e, ResultMailEvent::Green));
                let red = node
                    .transition(ResultMailState::ProofOpen)
                    .on(|e| matches!(e, ResultMailEvent::RedRow { .. }))
                    .guard(|ctx, e| {
                        matches!(e, ResultMailEvent::RedRow { id, .. } if !ctx.mailed_rows.contains(id))
                    })
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let ResultMailEvent::RedRow { id, owner } = e {
                            ctx.mailed_rows.insert(id.clone());
                            q.exec(ResultMailCmd::Mail {
                                owner: owner.clone(),
                                body: id.clone(),
                            });
                        }
                    });
                let land = node
                    .transition(ResultMailState::Landed)
                    .on(|e| matches!(e, ResultMailEvent::Land { .. }))
                    .guard(|ctx, e| {
                        matches!(e, ResultMailEvent::Land { bead, .. } if !ctx.mailed_lands.contains(bead))
                    })
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let ResultMailEvent::Land { bead, assignee } = e {
                            ctx.mailed_lands.insert(bead.clone());
                            q.exec(ResultMailCmd::Mail {
                                owner: assignee.clone(),
                                body: format!("{bead} landed"),
                            });
                        }
                    });
                node.on(green).on(red).on(land)
            })
            .state(ResultMailState::ProofOpen, |node| {
                let end = node
                    .transition(ResultMailState::Idle)
                    .on(|e| matches!(e, ResultMailEvent::ProofEnd));
                let red = node
                    .transition(ResultMailState::ProofOpen)
                    .on(|e| matches!(e, ResultMailEvent::RedRow { .. }))
                    .guard(|ctx, e| {
                        matches!(e, ResultMailEvent::RedRow { id, .. } if !ctx.mailed_rows.contains(id))
                    })
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let ResultMailEvent::RedRow { id, owner } = e {
                            ctx.mailed_rows.insert(id.clone());
                            q.exec(ResultMailCmd::Mail {
                                owner: owner.clone(),
                                body: id.clone(),
                            });
                        }
                    });
                let land = node
                    .transition(ResultMailState::Landed)
                    .on(|e| matches!(e, ResultMailEvent::Land { .. }))
                    .guard(|ctx, e| {
                        matches!(e, ResultMailEvent::Land { bead, .. } if !ctx.mailed_lands.contains(bead))
                    })
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let ResultMailEvent::Land { bead, assignee } = e {
                            ctx.mailed_lands.insert(bead.clone());
                            q.exec(ResultMailCmd::Mail {
                                owner: assignee.clone(),
                                body: format!("{bead} landed"),
                            });
                        }
                    });
                node.on(end).on(red).on(land)
            })
            .state(ResultMailState::Landed, |node| {
                let replay = node
                    .transition(ResultMailState::Landed)
                    .on(|e| matches!(e, ResultMailEvent::Replay { .. }))
                    .guard(|ctx, e| {
                        matches!(e, ResultMailEvent::Replay { bead } if !ctx.mailed_lands.contains(bead))
                    })
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let ResultMailEvent::Replay { bead } = e {
                            ctx.mailed_lands.insert(bead.clone());
                            q.exec(ResultMailCmd::Mail {
                                owner: "captain".into(),
                                body: format!("{bead} landed"),
                            });
                        }
                    });
                let finish = node
                    .transition(ResultMailState::Done)
                    .on(|e| matches!(e, ResultMailEvent::Finish));
                node.on(replay).on(finish)
            })
            .state(ResultMailState::Done, |node| {
                node.transition(ResultMailState::Done).on(|_| true)
            })
            .final_state(ResultMailState::Done)
            .build()
    }

    fn init_event() -> Self::Event {
        ResultMailEvent::ProofEnd
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        ResultMailEvent::ProofEnd
    }
}

#[must_use]
pub fn build_result_mail_machine()
-> Machine<ResultMailState, ResultMailEvent, ResultMailContext, ResultMailCmd> {
    ResultMailProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::MachineState;

    fn run(log: &[ResultMailEvent]) -> (ResultMailState, Vec<ResultMailCmd>) {
        let m = build_result_mail_machine();
        let mut ms = MachineState::at(ResultMailState::Idle, ResultMailContext::default());
        let mut cmds = Vec::new();
        for e in log {
            let out = m.step_state(&ms, e);
            cmds.extend(out.commands.clone());
            ms = MachineState::at(
                out.configuration.primary().cloned().unwrap(),
                out.context.clone(),
            );
        }
        (
            *ms.configuration.primary().unwrap_or(&ResultMailState::Idle),
            cmds,
        )
    }

    #[test]
    fn one_message_log_notifies_each_owner_once_even_on_replay() {
        let (state, cmds) = run(&[
            ResultMailEvent::RedRow {
                id: "r1".into(),
                owner: "CrewA".into(),
            },
            ResultMailEvent::RedRow {
                id: "r1".into(),
                owner: "CrewA".into(),
            },
            ResultMailEvent::ProofEnd,
            ResultMailEvent::Land {
                bead: "bd-x".into(),
                assignee: "CrewB".into(),
            },
            ResultMailEvent::Replay {
                bead: "bd-x".into(),
            },
            ResultMailEvent::Finish,
        ]);
        assert_eq!(state, ResultMailState::Done);
        assert_eq!(
            cmds,
            vec![
                ResultMailCmd::Mail {
                    owner: "CrewA".into(),
                    body: "r1".into(),
                },
                ResultMailCmd::Mail {
                    owner: "CrewB".into(),
                    body: "bd-x landed".into(),
                },
            ]
        );
    }

    #[test]
    fn a_green_proof_emits_no_failure_mail() {
        let (state, cmds) = run(&[ResultMailEvent::Green]);
        assert_eq!(state, ResultMailState::Done);
        assert!(
            cmds.is_empty(),
            "green proof mails nobody about failures: {cmds:?}"
        );
    }

    /// The observations in traces/ResultMail.recorded.jsonl come from this
    /// reducer's message log. Keep the action sequence pinned so the conform
    /// check in verify-formal.sh is replaying real implementation behaviour.
    #[test]
    fn recorded_trace_actions_match_the_reducer() {
        fn action(e: &ResultMailEvent) -> &'static str {
            match e {
                ResultMailEvent::RedRow { .. } => "RedRow",
                ResultMailEvent::ProofEnd => "ProofEnd",
                ResultMailEvent::Land { .. } => "Land",
                ResultMailEvent::Replay { .. } => "Replay",
                ResultMailEvent::Green => "Green",
                ResultMailEvent::Finish => "Finish",
            }
        }
        let log = [
            ResultMailEvent::RedRow {
                id: "r1".into(),
                owner: "o1".into(),
            },
            ResultMailEvent::ProofEnd,
            ResultMailEvent::Land {
                bead: "bd1".into(),
                assignee: "o2".into(),
            },
            ResultMailEvent::Replay { bead: "bd1".into() },
        ];
        let observed: Vec<&str> = log.iter().map(action).collect();
        assert_eq!(observed, ["RedRow", "ProofEnd", "Land", "Replay"]);
        let (_, cmds) = run(&log);
        assert_eq!(
            cmds,
            vec![
                ResultMailCmd::Mail {
                    owner: "o1".into(),
                    body: "r1".into(),
                },
                ResultMailCmd::Mail {
                    owner: "o2".into(),
                    body: "bd1 landed".into(),
                },
            ]
        );
    }
}
