//! Mail-send outage statechart (bd-ys3jp).
//!
//! One send from acceptance to delivery. The queue row is journaled BEFORE the
//! transport is tried; a retry keeps the same corr, so the fold can never emit
//! two Deliver commands for one send.

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MailQueueState {
    Idle,
    Queued,
    Flushing,
    Delivered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailQueueEvent {
    Send { corr: String },
    DaemonDown,
    DaemonUp,
    FlushOk,
    FlushFail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MailQueueContext {
    pub corr: String,
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailQueueCmd {
    Journal { corr: String },
    Deliver { corr: String },
    Drop { corr: String },
}

impl CmdTrait for MailQueueCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct MailQueueProgramDef;

impl StatechartDefinition for MailQueueProgramDef {
    type State = MailQueueState;
    type Event = MailQueueEvent;
    type Context = MailQueueContext;
    type Cmd = MailQueueCmd;
    type Msg = MailQueueEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(MailQueueState::Idle, MailQueueContext::default())
            .state(MailQueueState::Idle, |node| {
                node.transition(MailQueueState::Queued)
                    .on(|e| matches!(e, MailQueueEvent::Send { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let MailQueueEvent::Send { corr } = e {
                            ctx.corr = corr.clone();
                            q.exec(MailQueueCmd::Journal { corr: corr.clone() });
                        }
                    })
            })
            .state(MailQueueState::Queued, |node| {
                let same = node
                    .transition(MailQueueState::Queued)
                    .on(|e| matches!(e, MailQueueEvent::Send { .. } | MailQueueEvent::DaemonDown));
                let up = node
                    .transition(MailQueueState::Flushing)
                    .on(|e| matches!(e, MailQueueEvent::DaemonUp))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.attempts += 1;
                        q.exec(MailQueueCmd::Deliver {
                            corr: ctx.corr.clone(),
                        });
                    });
                node.on(same).on(up)
            })
            .state(MailQueueState::Flushing, |node| {
                let ok = node
                    .transition(MailQueueState::Delivered)
                    .on(|e| matches!(e, MailQueueEvent::FlushOk))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        q.exec(MailQueueCmd::Drop {
                            corr: ctx.corr.clone(),
                        });
                    });
                let fail = node
                    .transition(MailQueueState::Queued)
                    .on(|e| matches!(e, MailQueueEvent::FlushFail | MailQueueEvent::DaemonDown));
                node.on(ok).on(fail)
            })
            .state(MailQueueState::Delivered, |node| {
                node.transition(MailQueueState::Delivered).on(|_| true)
            })
            .final_state(MailQueueState::Delivered)
            .build()
    }

    fn init_event() -> Self::Event {
        MailQueueEvent::DaemonUp
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        MailQueueEvent::FlushFail
    }
}

#[must_use]
pub fn build_mail_queue_machine()
-> Machine<MailQueueState, MailQueueEvent, MailQueueContext, MailQueueCmd> {
    MailQueueProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::MachineState;

    fn run(log: &[MailQueueEvent]) -> (MailQueueState, Vec<MailQueueCmd>) {
        let m = build_mail_queue_machine();
        let mut ms = MachineState::at(MailQueueState::Idle, MailQueueContext::default());
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
            *ms.configuration.primary().unwrap_or(&MailQueueState::Idle),
            cmds,
        )
    }

    #[test]
    fn one_message_log_yields_one_journal_one_deliver_one_drop() {
        let log = [
            MailQueueEvent::Send { corr: "c-1".into() },
            MailQueueEvent::DaemonDown,
            MailQueueEvent::Send { corr: "c-1".into() },
            MailQueueEvent::DaemonUp,
            MailQueueEvent::FlushOk,
        ];
        let (state, cmds) = run(&log);
        assert_eq!(state, MailQueueState::Delivered);
        assert_eq!(
            cmds,
            vec![
                MailQueueCmd::Journal { corr: "c-1".into() },
                MailQueueCmd::Deliver { corr: "c-1".into() },
                MailQueueCmd::Drop { corr: "c-1".into() },
            ]
        );
    }

    #[test]
    fn a_failed_flush_requeues_with_the_same_corr() {
        let (state, cmds) = run(&[
            MailQueueEvent::Send { corr: "c-2".into() },
            MailQueueEvent::DaemonUp,
            MailQueueEvent::FlushFail,
            MailQueueEvent::DaemonUp,
            MailQueueEvent::FlushOk,
        ]);
        assert_eq!(state, MailQueueState::Delivered);
        assert_eq!(
            cmds,
            vec![
                MailQueueCmd::Journal { corr: "c-2".into() },
                MailQueueCmd::Deliver { corr: "c-2".into() },
                MailQueueCmd::Deliver { corr: "c-2".into() },
                MailQueueCmd::Drop { corr: "c-2".into() },
            ]
        );
    }
}
