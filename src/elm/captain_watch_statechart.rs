//! Captain-watch event stream (bd-7ky1v).
//!
//! One-shot watch exits on the first event. Stream mode prints one line per
//! event and keeps watching: a harness line-monitor wakes the captain per line
//! instead of the captain re-arming the watch after every resume.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CaptainWatchState {
    Watching,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptainWatchEvent {
    Mail { id: String },
    Stall { id: String },
    Preflight { id: String },
    ProofDone { id: String },
    Finish,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CaptainWatchContext {
    pub stream: bool,
    pub emitted: BTreeSet<String>,
    pub exits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptainWatchCmd {
    EmitLine { text: String },
    Exit { code: i32 },
}

impl CmdTrait for CaptainWatchCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct CaptainWatchProgramDef;

impl StatechartDefinition for CaptainWatchProgramDef {
    type State = CaptainWatchState;
    type Event = CaptainWatchEvent;
    type Context = CaptainWatchContext;
    type Cmd = CaptainWatchCmd;
    type Msg = CaptainWatchEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(CaptainWatchState::Watching, CaptainWatchContext::default())
            .state(CaptainWatchState::Watching, |node| {
                fn fire(
                    ctx: &mut CaptainWatchContext,
                    q: &mut ActionQueue<CaptainWatchCmd, CaptainWatchEvent>,
                    kind: &str,
                    id: &str,
                ) {
                    if ctx.stream {
                        if ctx.emitted.insert(format!("{kind}:{id}")) {
                            q.exec(CaptainWatchCmd::EmitLine {
                                text: format!("{kind} {id}"),
                            });
                        }
                    } else if ctx.exits == 0 {
                        ctx.exits += 1;
                        q.exec(CaptainWatchCmd::Exit { code: 0 });
                    }
                }

                let mail = node
                    .transition(CaptainWatchState::Watching)
                    .on(|e| matches!(e, CaptainWatchEvent::Mail { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let CaptainWatchEvent::Mail { id } = e {
                            fire(ctx, q, "mail", id);
                        }
                    });
                let stall = node
                    .transition(CaptainWatchState::Watching)
                    .on(|e| matches!(e, CaptainWatchEvent::Stall { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let CaptainWatchEvent::Stall { id } = e {
                            fire(ctx, q, "stall", id);
                        }
                    });
                let preflight = node
                    .transition(CaptainWatchState::Watching)
                    .on(|e| matches!(e, CaptainWatchEvent::Preflight { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let CaptainWatchEvent::Preflight { id } = e {
                            fire(ctx, q, "preflight", id);
                        }
                    });
                let proof = node
                    .transition(CaptainWatchState::Watching)
                    .on(|e| matches!(e, CaptainWatchEvent::ProofDone { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let CaptainWatchEvent::ProofDone { id } = e {
                            fire(ctx, q, "proof", id);
                        }
                    });
                let finish = node
                    .transition(CaptainWatchState::Done)
                    .on(|e| matches!(e, CaptainWatchEvent::Finish));
                node.on(mail).on(stall).on(preflight).on(proof).on(finish)
            })
            .state(CaptainWatchState::Done, |node| {
                let stay = node.transition(CaptainWatchState::Done).on(|_| true);
                node.on(stay)
            })
            .final_state(CaptainWatchState::Done, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        CaptainWatchEvent::Finish
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        CaptainWatchEvent::Finish
    }
}

#[must_use]
pub fn build_captain_watch_machine()
-> Machine<CaptainWatchState, CaptainWatchEvent, CaptainWatchContext, CaptainWatchCmd> {
    CaptainWatchProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::MachineState;

    fn run(stream: bool, log: &[CaptainWatchEvent]) -> Vec<CaptainWatchCmd> {
        let m = build_captain_watch_machine();
        let mut ms = MachineState::at(
            CaptainWatchState::Watching,
            CaptainWatchContext {
                stream,
                ..CaptainWatchContext::default()
            },
        );
        let mut cmds = Vec::new();
        for e in log {
            let out = m.step_state(&ms, e);
            cmds.extend(out.commands.clone());
            ms = MachineState::at(
                out.configuration.primary().cloned().unwrap(),
                out.context.clone(),
            );
        }
        cmds
    }

    #[test]
    fn stream_emits_one_line_per_event_and_never_exits() {
        let log = [
            CaptainWatchEvent::Mail { id: "m1".into() },
            CaptainWatchEvent::Mail { id: "m1".into() },
            CaptainWatchEvent::Stall { id: "s1".into() },
            CaptainWatchEvent::Preflight { id: "p1".into() },
            CaptainWatchEvent::ProofDone { id: "pd1".into() },
        ];
        let cmds = run(true, &log);
        assert_eq!(
            cmds,
            vec![
                CaptainWatchCmd::EmitLine {
                    text: "mail m1".into()
                },
                CaptainWatchCmd::EmitLine {
                    text: "stall s1".into()
                },
                CaptainWatchCmd::EmitLine {
                    text: "preflight p1".into()
                },
                CaptainWatchCmd::EmitLine {
                    text: "proof pd1".into()
                },
            ]
        );
    }

    #[test]
    fn one_shot_exits_on_the_first_event_unchanged() {
        let cmds = run(
            false,
            &[
                CaptainWatchEvent::Mail { id: "m1".into() },
                CaptainWatchEvent::Stall { id: "s1".into() },
            ],
        );
        assert_eq!(cmds, vec![CaptainWatchCmd::Exit { code: 0 }]);
    }

    /// traces/CaptainWatch.recorded.jsonl is recorded from this reducer's log;
    /// pin the action sequence so the conform check replays real behaviour.
    #[test]
    fn recorded_trace_actions_match_the_reducer() {
        fn action(e: &CaptainWatchEvent) -> &'static str {
            match e {
                CaptainWatchEvent::Mail { .. } => "Mail",
                CaptainWatchEvent::Stall { .. } => "Stall",
                CaptainWatchEvent::Preflight { .. } => "Preflight",
                CaptainWatchEvent::ProofDone { .. } => "ProofDone",
                CaptainWatchEvent::Finish => "Finish",
            }
        }
        let log = [
            CaptainWatchEvent::Mail { id: "m1".into() },
            CaptainWatchEvent::Stall { id: "s1".into() },
            CaptainWatchEvent::Preflight { id: "p1".into() },
            CaptainWatchEvent::ProofDone { id: "pd1".into() },
        ];
        let observed: Vec<&str> = log.iter().map(action).collect();
        assert_eq!(observed, ["Mail", "Stall", "Preflight", "ProofDone"]);
        assert_eq!(
            run(true, &log).len(),
            4,
            "one line per event in stream mode"
        );
    }
}
