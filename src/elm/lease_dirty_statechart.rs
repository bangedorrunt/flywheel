//! Dirty-lease lifecycle statechart (bd-nmilo).
//!
//! A live hold survives its TTL only while the holder still has uncommitted
//! changes in the path. Commit starts a short grace; past it the lease expires
//! and the waitlist hands the path over. Pure fold; mirrors the dirty half of
//! toron's `ToronLeaseResolution.tla`.

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DirtyLeaseState {
    Clean,
    Dirty,
    Grace,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirtyLeaseEvent {
    Edit { path: String },
    Commit { path: String },
    Tick,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DirtyLeaseContext {
    pub holder: String,
    pub path: String,
    pub grace_left: u32,
    pub handoffs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirtyLeaseCmd {
    Expire { holder: String, path: String },
    Handoff { path: String, to: String },
    Journal { key: String },
}

impl CmdTrait for DirtyLeaseCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct DirtyLeaseProgramDef;

impl StatechartDefinition for DirtyLeaseProgramDef {
    type State = DirtyLeaseState;
    type Event = DirtyLeaseEvent;
    type Context = DirtyLeaseContext;
    type Cmd = DirtyLeaseCmd;
    type Msg = DirtyLeaseEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(DirtyLeaseState::Clean, DirtyLeaseContext::default())
            .state(DirtyLeaseState::Clean, |node| {
                let edit = node
                    .transition(DirtyLeaseState::Dirty)
                    .on(|e| matches!(e, DirtyLeaseEvent::Edit { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let DirtyLeaseEvent::Edit { path } = e {
                            ctx.path = path.clone();
                            q.exec(DirtyLeaseCmd::Journal {
                                key: format!("lease:dirty:{path}"),
                            });
                        }
                    });
                let release = node
                    .transition(DirtyLeaseState::Released)
                    .on(|e| matches!(e, DirtyLeaseEvent::Release));
                node.on(edit).on(release)
            })
            .state(DirtyLeaseState::Dirty, |node| {
                let commit = node
                    .transition(DirtyLeaseState::Grace)
                    .on(|e| matches!(e, DirtyLeaseEvent::Commit { .. }))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.grace_left = 1;
                        q.exec(DirtyLeaseCmd::Journal {
                            key: format!("lease:clean:{}", ctx.path),
                        });
                    });
                let edit = node
                    .transition(DirtyLeaseState::Dirty)
                    .on(|e| matches!(e, DirtyLeaseEvent::Edit { .. }));
                let release = node
                    .transition(DirtyLeaseState::Released)
                    .on(|e| matches!(e, DirtyLeaseEvent::Release));
                node.on(commit).on(edit).on(release)
            })
            .state(DirtyLeaseState::Grace, |node| {
                let tick = node
                    .transition(DirtyLeaseState::Released)
                    .on(|e| matches!(e, DirtyLeaseEvent::Tick))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        q.exec(DirtyLeaseCmd::Expire {
                            holder: ctx.holder.clone(),
                            path: ctx.path.clone(),
                        });
                        if let Some(next) = ctx.handoffs.first().cloned() {
                            q.exec(DirtyLeaseCmd::Handoff {
                                path: ctx.path.clone(),
                                to: next,
                            });
                        }
                        q.exec(DirtyLeaseCmd::Journal {
                            key: format!("lease:handoff:{}", ctx.path),
                        });
                        ctx.grace_left = 0;
                    });
                let edit = node
                    .transition(DirtyLeaseState::Dirty)
                    .on(|e| matches!(e, DirtyLeaseEvent::Edit { .. }))
                    .action(|ctx, _, _q: &mut ActionQueue<_, _>| ctx.grace_left = 0);
                node.on(tick).on(edit)
            })
            .final_state(DirtyLeaseState::Released, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        DirtyLeaseEvent::Release
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        DirtyLeaseEvent::Release
    }
}

#[must_use]
pub fn build_dirty_lease_machine()
-> Machine<DirtyLeaseState, DirtyLeaseEvent, DirtyLeaseContext, DirtyLeaseCmd> {
    DirtyLeaseProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::MachineState;

    #[test]
    fn one_message_log_expires_only_after_clean_and_idle() {
        let m = build_dirty_lease_machine();
        let mut ctx = DirtyLeaseContext {
            holder: "A".into(),
            handoffs: vec!["B".into()],
            ..Default::default()
        };
        ctx.path = "src/lib.rs".into();
        let mut ms = MachineState::at(DirtyLeaseState::Clean, ctx);
        let events = [
            DirtyLeaseEvent::Edit {
                path: "src/lib.rs".into(),
            },
            DirtyLeaseEvent::Commit {
                path: "src/lib.rs".into(),
            },
            DirtyLeaseEvent::Tick,
        ];
        let mut cmds = Vec::new();
        for e in events {
            let out = m.step_state(&ms, &e);
            cmds.extend(out.commands.clone());
            ms = MachineState::at(out.configuration.primary().cloned().unwrap(), out.context);
        }
        assert_eq!(ms.configuration.primary(), Some(&DirtyLeaseState::Released));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, DirtyLeaseCmd::Expire { .. }))
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, DirtyLeaseCmd::Handoff { to, .. } if to == "B"))
        );

        let ms2 = MachineState::at(
            DirtyLeaseState::Dirty,
            DirtyLeaseContext {
                holder: "A".into(),
                path: "src/lib.rs".into(),
                handoffs: vec!["B".into()],
                grace_left: 0,
            },
        );
        let out = m.step_state(&ms2, &DirtyLeaseEvent::Tick);
        assert!(
            !out.commands
                .iter()
                .any(|c| matches!(c, DirtyLeaseCmd::Expire { .. })),
            "a dirty live holder is never expired"
        );
    }
}
