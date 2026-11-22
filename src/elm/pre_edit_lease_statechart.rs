//! Pre-edit lease statechart (bd-lk0kt).
//!
//! One file at the moment a seat edits it. The lease is taken before the write;
//! a conflict queues exactly one waiter and names the holder; a repeat edit by
//! the holder is a no-op.

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PreEditState {
    Idle,
    Leased,
    Blocked,
    Queued,
    Delivered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreEditEvent {
    PreEdit { holder: String },
    Conflict { holder: String },
    Handoff,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PreEditContext {
    pub holder: String,
    pub waiters: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreEditCmd {
    Acquire { holder: String },
    Queue { holder: String },
    Write { holder: String },
}

impl CmdTrait for PreEditCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct PreEditProgramDef;

impl StatechartDefinition for PreEditProgramDef {
    type State = PreEditState;
    type Event = PreEditEvent;
    type Context = PreEditContext;
    type Cmd = PreEditCmd;
    type Msg = PreEditEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(PreEditState::Idle, PreEditContext::default())
            .state(PreEditState::Idle, |node| {
                let grant = node
                    .transition(PreEditState::Leased)
                    .on(|e| matches!(e, PreEditEvent::PreEdit { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let PreEditEvent::PreEdit { holder } = e {
                            ctx.holder = holder.clone();
                        }
                        q.exec(PreEditCmd::Acquire {
                            holder: ctx.holder.clone(),
                        });
                    });
                node.on(grant)
            })
            .state(PreEditState::Leased, |node| {
                let repeat = node
                    .transition(PreEditState::Leased)
                    .on(|e| matches!(e, PreEditEvent::PreEdit { .. }));
                let conflict = node
                    .transition(PreEditState::Blocked)
                    .on(|e| matches!(e, PreEditEvent::Conflict { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let PreEditEvent::Conflict { holder } = e {
                            ctx.holder = holder.clone();
                        }
                        q.exec(PreEditCmd::Queue {
                            holder: ctx.holder.clone(),
                        });
                    });
                let write = node
                    .transition(PreEditState::Delivered)
                    .on(|e| matches!(e, PreEditEvent::Write))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        q.exec(PreEditCmd::Write {
                            holder: ctx.holder.clone(),
                        });
                    });
                node.on(repeat).on(conflict).on(write)
            })
            .state(PreEditState::Blocked, |node| {
                let queue = node
                    .transition(PreEditState::Queued)
                    .on(|e| matches!(e, PreEditEvent::Conflict { .. }))
                    .action(|ctx, e, _q: &mut ActionQueue<_, _>| {
                        if let PreEditEvent::Conflict { holder } = e {
                            ctx.holder = holder.clone();
                        }
                        ctx.waiters += 1;
                    });
                let handoff = node
                    .transition(PreEditState::Leased)
                    .on(|e| matches!(e, PreEditEvent::Handoff));
                node.on(queue).on(handoff)
            })
            .state(PreEditState::Queued, |node| {
                let handoff = node
                    .transition(PreEditState::Leased)
                    .on(|e| matches!(e, PreEditEvent::Handoff));
                node.on(handoff)
            })
            .final_state(PreEditState::Delivered, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        PreEditEvent::PreEdit {
            holder: String::new(),
        }
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        PreEditEvent::Conflict {
            holder: String::new(),
        }
    }
}

#[must_use]
pub fn build_pre_edit_machine() -> Machine<PreEditState, PreEditEvent, PreEditContext, PreEditCmd> {
    PreEditProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::MachineState;

    #[test]
    fn pre_edit_message_log_replays_to_one_lease_and_one_write() {
        let machine = build_pre_edit_machine();
        let log = [
            PreEditEvent::PreEdit {
                holder: "Alpha".into(),
            },
            PreEditEvent::Conflict {
                holder: "Beta".into(),
            },
            PreEditEvent::Handoff,
            PreEditEvent::Write,
        ];
        let mut state = MachineState::at(PreEditState::Idle, PreEditContext::default());
        let mut cmds = Vec::new();
        for event in log {
            let out = machine.step_state(&state, &event);
            cmds.extend(out.commands.clone());
            state = MachineState::at(
                out.configuration.primary().cloned().unwrap(),
                out.context.clone(),
            );
        }
        assert_eq!(
            *state.configuration.primary().unwrap(),
            PreEditState::Delivered
        );
        assert_eq!(
            cmds,
            vec![
                PreEditCmd::Acquire {
                    holder: "Alpha".into()
                },
                PreEditCmd::Queue {
                    holder: "Beta".into()
                },
                PreEditCmd::Write {
                    holder: "Beta".into()
                },
            ]
        );
    }
}
