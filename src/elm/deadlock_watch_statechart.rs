//! Deadlock watch statechart (bd-v4dcv).
//!
//! One watch sweep from idle to reported. A scan that finds a cycle or an
//! overdue wait emits exactly one typed notify per new finding; a quiet
//! sweep journals and returns to idle. Pure fold; commands are descriptive
//! only. Mirrors `DeadlockWatch.tla`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum WatchState {
    Idle,
    Scanning,
    Alerted,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchEvent {
    Scan,
    CycleFound { key: String },
    WaitFound { key: String },
    Quiet,
    Ack,
    Retire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WatchContext {
    pub pending: Vec<String>,
    pub notified: Vec<String>,
    pub reported: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchCmd {
    Notify { key: String },
    Journal { key: String },
}

impl CmdTrait for WatchCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct WatchProgramDef;

impl StatechartDefinition for WatchProgramDef {
    type State = WatchState;
    type Event = WatchEvent;
    type Context = WatchContext;
    type Cmd = WatchCmd;
    type Msg = WatchEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(WatchState::Idle, WatchContext::default())
            .state(WatchState::Idle, |node| {
                let scan = node
                    .transition(WatchState::Scanning)
                    .on(|e| matches!(e, WatchEvent::Scan))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.pending.clear();
                        q.exec(WatchCmd::Journal {
                            key: "deadlock:scan".into(),
                        });
                    });
                let retire = node
                    .transition(WatchState::Retired)
                    .on(|e| matches!(e, WatchEvent::Retire));
                node.on(scan).on(retire)
            })
            .state(WatchState::Scanning, |node| {
                let cycle = node
                    .transition(WatchState::Alerted)
                    .on(|e| matches!(e, WatchEvent::CycleFound { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let WatchEvent::CycleFound { key } = e {
                            if ctx.reported.insert(key.clone()) {
                                ctx.pending.push(key.clone());
                                q.exec(WatchCmd::Notify { key: key.clone() });
                            }
                            q.exec(WatchCmd::Journal {
                                key: format!("deadlock:cycle:{key}"),
                            });
                        }
                    });
                let wait = node
                    .transition(WatchState::Alerted)
                    .on(|e| matches!(e, WatchEvent::WaitFound { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let WatchEvent::WaitFound { key } = e {
                            if ctx.reported.insert(key.clone()) {
                                ctx.pending.push(key.clone());
                                q.exec(WatchCmd::Notify { key: key.clone() });
                            }
                            q.exec(WatchCmd::Journal {
                                key: format!("deadlock:wait:{key}"),
                            });
                        }
                    });
                let quiet = node
                    .transition(WatchState::Idle)
                    .on(|e| matches!(e, WatchEvent::Quiet))
                    .action(|_ctx, _, q: &mut ActionQueue<_, _>| {
                        q.exec(WatchCmd::Journal {
                            key: "deadlock:quiet".into(),
                        });
                    });
                let retire = node
                    .transition(WatchState::Retired)
                    .on(|e| matches!(e, WatchEvent::Retire));
                node.on(cycle).on(wait).on(quiet).on(retire)
            })
            .state(WatchState::Alerted, |node| {
                let more = node
                    .transition(WatchState::Alerted)
                    .on(|e| {
                        matches!(
                            e,
                            WatchEvent::CycleFound { .. } | WatchEvent::WaitFound { .. }
                        )
                    })
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        let key = match e {
                            WatchEvent::CycleFound { key } | WatchEvent::WaitFound { key } => {
                                key.clone()
                            }
                            _ => return,
                        };
                        if ctx.reported.insert(key.clone()) {
                            ctx.pending.push(key.clone());
                            q.exec(WatchCmd::Notify { key: key.clone() });
                        }
                        q.exec(WatchCmd::Journal {
                            key: format!("deadlock:more:{key}"),
                        });
                    });
                let ack = node
                    .transition(WatchState::Idle)
                    .on(|e| matches!(e, WatchEvent::Ack))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.pending.clear();
                        q.exec(WatchCmd::Journal {
                            key: "deadlock:ack".into(),
                        });
                    });
                let retire = node
                    .transition(WatchState::Retired)
                    .on(|e| matches!(e, WatchEvent::Retire));
                node.on(more).on(ack).on(retire)
            })
            .final_state(WatchState::Retired, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        WatchEvent::Retire
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        WatchEvent::Retire
    }
}

#[must_use]
pub fn build_deadlock_watch_machine() -> Machine<WatchState, WatchEvent, WatchContext, WatchCmd> {
    WatchProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One message log: a scan finds a cycle, a second finding joins it, a
    /// repeat of the first finding is silent (already reported), and the ack
    /// returns to idle with an empty pending list.
    #[test]
    fn one_message_log_alerts_once_per_finding() {
        use toron_core::kernel::MachineState;

        let m = build_deadlock_watch_machine();
        let mut ms = MachineState::at(WatchState::Idle, WatchContext::default());
        let mut cmds = Vec::new();
        let feed = |ms: &MachineState<WatchState, WatchContext>,
                    event: &WatchEvent,
                    cmds: &mut Vec<WatchCmd>| {
            let out = m.step_state(ms, event);
            cmds.extend(out.commands.clone());
            MachineState::at(
                out.configuration
                    .primary()
                    .cloned()
                    .unwrap_or(WatchState::Idle),
                out.context,
            )
        };
        ms = feed(&ms, &WatchEvent::Scan, &mut cmds);
        assert_eq!(ms.configuration.primary(), Some(&WatchState::Scanning));

        ms = feed(
            &ms,
            &WatchEvent::CycleFound {
                key: "cycle:A>B".into(),
            },
            &mut cmds,
        );
        ms = feed(
            &ms,
            &WatchEvent::WaitFound {
                key: "wait:C>D".into(),
            },
            &mut cmds,
        );
        assert_eq!(ms.configuration.primary(), Some(&WatchState::Alerted));
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, WatchCmd::Notify { .. }))
                .count(),
            2,
            "one typed notify per new finding: {cmds:?}"
        );

        ms = feed(
            &ms,
            &WatchEvent::CycleFound {
                key: "cycle:A>B".into(),
            },
            &mut cmds,
        );
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, WatchCmd::Notify { .. }))
                .count(),
            2,
            "a repeat of a reported finding rings no second time: {cmds:?}"
        );

        ms = feed(&ms, &WatchEvent::Ack, &mut cmds);
        assert_eq!(ms.configuration.primary(), Some(&WatchState::Idle));
        assert!(ms.context.pending.is_empty());
    }

    /// A quiet sweep journals and returns to idle with no notify.
    #[test]
    fn a_quiet_sweep_journals_and_returns() {
        use toron_core::kernel::MachineState;

        let m = build_deadlock_watch_machine();
        let mut ms = MachineState::at(WatchState::Idle, WatchContext::default());
        let mut cmds = Vec::new();
        let out = m.step_state(&ms, &WatchEvent::Scan);
        cmds.extend(out.commands.clone());
        ms = MachineState::at(
            out.configuration
                .primary()
                .cloned()
                .unwrap_or(WatchState::Idle),
            out.context,
        );
        let out = m.step_state(&ms, &WatchEvent::Quiet);
        cmds.extend(out.commands.clone());
        assert!(cmds.iter().all(|c| !matches!(c, WatchCmd::Notify { .. })));
    }
}
