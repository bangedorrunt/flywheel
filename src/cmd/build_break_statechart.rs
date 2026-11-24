//! Build-break alert statechart (bd-6xu6r).
//!
//! A broken shared build mails the holder of the failing file exactly once
//! per break. A repeat of the same break emits nothing, and an error in the
//! caller's own file never mails the caller. The dedupe set lives in the
//! context, so replaying a message log is pure.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BuildBreakState {
    Idle,
    Alerted,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildBreakEvent {
    /// A compiler error attributed to `file`, seen by `caller`.
    Broken {
        key: String,
        file: String,
        snippet: String,
        holder: String,
        caller: String,
    },
    /// The break is gone; the same key may alert again if it returns.
    Recovered {
        key: String,
    },
    Finish,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BuildBreakContext {
    pub alerted: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildBreakCmd {
    Mail { owner: String, body: String },
    Journal { key: String },
}

impl CmdTrait for BuildBreakCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct BuildBreakProgramDef;

impl StatechartDefinition for BuildBreakProgramDef {
    type State = BuildBreakState;
    type Event = BuildBreakEvent;
    type Context = BuildBreakContext;
    type Cmd = BuildBreakCmd;
    type Msg = BuildBreakEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        fn alert(
            ctx: &mut BuildBreakContext,
            q: &mut ActionQueue<BuildBreakCmd, BuildBreakEvent>,
            key: &str,
            file: &str,
            snippet: &str,
            holder: &str,
        ) {
            ctx.alerted.insert(key.to_string());
            q.exec(BuildBreakCmd::Mail {
                owner: holder.to_string(),
                body: format!("build break in {file}\n{snippet}"),
            });
            q.exec(BuildBreakCmd::Journal {
                key: key.to_string(),
            });
        }
        Machine::builder(BuildBreakState::Idle, BuildBreakContext::default())
            .state(BuildBreakState::Idle, |node| {
                let broken = node
                    .transition(BuildBreakState::Alerted)
                    .on(|e| matches!(e, BuildBreakEvent::Broken { .. }))
                    .guard(|ctx, e| {
                        matches!(e, BuildBreakEvent::Broken { key, holder, caller, .. }
                            if !ctx.alerted.contains(key) && holder != caller)
                    })
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let BuildBreakEvent::Broken { key, file, snippet, holder, .. } = e {
                            alert(ctx, q, key, file, snippet, holder);
                        }
                    });
                let own = node
                    .transition(BuildBreakState::Idle)
                    .on(|e| matches!(e, BuildBreakEvent::Broken { .. }))
                    .guard(|_, e| {
                        matches!(e, BuildBreakEvent::Broken { holder, caller, .. } if holder == caller)
                    });
                node.on(broken).on(own)
            })
            .state(BuildBreakState::Alerted, |node| {
                let broken = node
                    .transition(BuildBreakState::Alerted)
                    .on(|e| matches!(e, BuildBreakEvent::Broken { .. }))
                    .guard(|ctx, e| {
                        matches!(e, BuildBreakEvent::Broken { key, holder, caller, .. }
                            if !ctx.alerted.contains(key) && holder != caller)
                    })
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let BuildBreakEvent::Broken { key, file, snippet, holder, .. } = e {
                            alert(ctx, q, key, file, snippet, holder);
                        }
                    });
                let recovered = node
                    .transition(BuildBreakState::Idle)
                    .on(|e| matches!(e, BuildBreakEvent::Recovered { .. }))
                    .action(|ctx, e, _: &mut ActionQueue<_, _>| {
                        if let BuildBreakEvent::Recovered { key } = e {
                            ctx.alerted.remove(key);
                        }
                    });
                node.on(broken).on(recovered)
            })
            .state(BuildBreakState::Done, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        BuildBreakEvent::Finish
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        BuildBreakEvent::Finish
    }
}

pub fn build_build_break_machine()
-> Machine<BuildBreakState, BuildBreakEvent, BuildBreakContext, BuildBreakCmd> {
    BuildBreakProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::MachineState;

    fn run(events: &[BuildBreakEvent]) -> (BuildBreakState, Vec<BuildBreakCmd>, BuildBreakContext) {
        let m = build_build_break_machine();
        let mut ms = MachineState::at(BuildBreakState::Idle, BuildBreakContext::default());
        let mut cmds = Vec::new();
        for e in events {
            let out = m.step_state(&ms, e);
            cmds.extend(out.commands.clone());
            ms = MachineState::at(out.configuration.primary().cloned().unwrap(), out.context);
        }
        (
            ms.configuration.primary().cloned().unwrap(),
            cmds,
            ms.context,
        )
    }

    fn broken(key: &str, holder: &str) -> BuildBreakEvent {
        BuildBreakEvent::Broken {
            key: key.into(),
            file: "src/one.rs".into(),
            snippet: "error[E0308]: mismatched types".into(),
            holder: holder.into(),
            caller: "PurpleRidge".into(),
        }
    }

    #[test]
    fn one_alert_per_break_and_never_self_alert() {
        let (_, cmds, ctx) = run(&[
            broken("break-1", "TealCove"),
            broken("break-1", "TealCove"),
            broken("break-2", "TealCove"),
            broken("break-3", "PurpleRidge"),
        ]);
        let mails: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                BuildBreakCmd::Mail { owner, body } => Some((owner.clone(), body.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            mails.len(),
            2,
            "one mail per distinct foreign break: {mails:?}"
        );
        assert!(mails.iter().all(|(owner, _)| owner == "TealCove"));
        assert!(
            mails
                .iter()
                .all(|(_, body)| body.contains("src/one.rs") && body.contains("E0308")),
            "the snippet must reach the owner: {mails:?}"
        );
        assert!(ctx.alerted.contains("break-1") && ctx.alerted.contains("break-2"));
        assert!(
            !ctx.alerted.contains("break-3"),
            "own break is never alerted"
        );
    }

    #[test]
    fn recovery_allows_a_new_alert_for_the_same_key() {
        let (_, cmds, _) = run(&[
            broken("break-1", "TealCove"),
            BuildBreakEvent::Recovered {
                key: "break-1".into(),
            },
            broken("break-1", "TealCove"),
        ]);
        let mails = cmds
            .iter()
            .filter(|c| matches!(c, BuildBreakCmd::Mail { .. }))
            .count();
        assert_eq!(mails, 2, "a recovered break may alert again");
    }
}
