//! Assign-and-brief statechart (bd-vk0do).
//!
//! One bead from unassigned to released. A held bead refuses a plain
//! assign with a typed refusal; `--reassign` replaces the holder and
//! notifies whoever it replaces; a newer brief withdraws the live one so
//! the crew never acts on a stale instruction. Pure fold; commands are
//! descriptive only. Mirrors `AssignBrief.tla`.

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BriefState {
    Idle,
    Held,
    Live,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BriefEvent {
    Assign { pin: String },
    Reassign { pin: String },
    Brief { id: String },
    Done { id: String },
    Release { source: String },
    Retire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BriefContext {
    pub holder: Option<String>,
    pub live_brief: Option<String>,
    pub withdrawn_briefs: Vec<String>,
    pub notified: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BriefCmd {
    Refuse { pin: String },
    Withdraw { id: String, pin: String },
    Notify { pin: String },
    Journal { key: String },
}

impl CmdTrait for BriefCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct BriefProgramDef;

impl StatechartDefinition for BriefProgramDef {
    type State = BriefState;
    type Event = BriefEvent;
    type Context = BriefContext;
    type Cmd = BriefCmd;
    type Msg = BriefEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(BriefState::Idle, BriefContext::default())
            .state(BriefState::Idle, |node| {
                let assign = node
                    .transition(BriefState::Held)
                    .on(|e| matches!(e, BriefEvent::Assign { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let BriefEvent::Assign { pin } = e {
                            ctx.holder = Some(pin.clone());
                            q.exec(BriefCmd::Journal {
                                key: format!("brief:grant:{pin}"),
                            });
                        }
                    });
                let retire = node
                    .transition(BriefState::Retired)
                    .on(|e| matches!(e, BriefEvent::Retire));
                node.on(assign).on(retire)
            })
            .state(BriefState::Held, |node| {
                let refuse = node
                    .transition(BriefState::Held)
                    .on(|e| matches!(e, BriefEvent::Assign { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let BriefEvent::Assign { pin } = e {
                            if ctx.holder.as_deref() != Some(pin.as_str()) {
                                q.exec(BriefCmd::Refuse { pin: pin.clone() });
                            }
                            q.exec(BriefCmd::Journal {
                                key: format!("brief:refused:{pin}"),
                            });
                        }
                    });
                let reassign = node
                    .transition(BriefState::Held)
                    .on(|e| matches!(e, BriefEvent::Reassign { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let BriefEvent::Reassign { pin } = e {
                            let old = ctx.holder.replace(pin.clone());
                            if let Some(old) = old
                                && old != *pin
                            {
                                ctx.notified.push(old.clone());
                                q.exec(BriefCmd::Notify { pin: old.clone() });
                                q.exec(BriefCmd::Journal {
                                    key: format!("brief:reassign:{old}>{pin}"),
                                });
                            }
                            q.exec(BriefCmd::Journal {
                                key: format!("brief:grant:{pin}"),
                            });
                        }
                    });
                let brief = node
                    .transition(BriefState::Live)
                    .on(|e| matches!(e, BriefEvent::Brief { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let BriefEvent::Brief { id } = e {
                            ctx.live_brief = Some(id.clone());
                            q.exec(BriefCmd::Journal {
                                key: format!("brief:open:{id}"),
                            });
                        }
                    });
                let release = node
                    .transition(BriefState::Idle)
                    .on(|e| matches!(e, BriefEvent::Release { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let BriefEvent::Release { source } = e {
                            ctx.holder = None;
                            q.exec(BriefCmd::Journal {
                                key: format!("brief:release:{source}"),
                            });
                        }
                    });
                let retire = node
                    .transition(BriefState::Retired)
                    .on(|e| matches!(e, BriefEvent::Retire));
                node.on(refuse)
                    .on(reassign)
                    .on(brief)
                    .on(release)
                    .on(retire)
            })
            .state(BriefState::Live, |node| {
                let supersede = node
                    .transition(BriefState::Live)
                    .on(|e| matches!(e, BriefEvent::Brief { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let BriefEvent::Brief { id } = e
                            && let Some(old) = ctx.live_brief.replace(id.clone())
                            && old != *id
                        {
                            ctx.withdrawn_briefs.push(old.clone());
                            if let Some(holder) = ctx.holder.clone() {
                                ctx.notified.push(holder.clone());
                                q.exec(BriefCmd::Withdraw {
                                    id: old.clone(),
                                    pin: holder,
                                });
                            }
                            q.exec(BriefCmd::Journal {
                                key: format!("brief:withdraw:{old}>{id}"),
                            });
                        }
                        if let BriefEvent::Brief { id } = e {
                            q.exec(BriefCmd::Journal {
                                key: format!("brief:open:{id}"),
                            });
                        }
                    });
                let done = node
                    .transition(BriefState::Held)
                    .on(|e| matches!(e, BriefEvent::Done { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let BriefEvent::Done { id } = e {
                            if ctx.live_brief.as_deref() == Some(id.as_str()) {
                                ctx.live_brief = None;
                            }
                            q.exec(BriefCmd::Journal {
                                key: format!("brief:done:{id}"),
                            });
                        }
                    });
                let release = node
                    .transition(BriefState::Idle)
                    .on(|e| matches!(e, BriefEvent::Release { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let BriefEvent::Release { source } = e {
                            ctx.holder = None;
                            ctx.live_brief = None;
                            q.exec(BriefCmd::Journal {
                                key: format!("brief:release:{source}"),
                            });
                        }
                    });
                let retire = node
                    .transition(BriefState::Retired)
                    .on(|e| matches!(e, BriefEvent::Retire));
                node.on(supersede).on(done).on(release).on(retire)
            })
            .final_state(BriefState::Retired, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        BriefEvent::Retire
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        BriefEvent::Retire
    }
}

#[must_use]
pub fn build_brief_machine() -> Machine<BriefState, BriefEvent, BriefContext, BriefCmd> {
    BriefProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One message log: a double assign refuses, a reassign notifies the
    /// replaced holder, and a newer brief withdraws the live one so the
    /// crew only ever sees one live instruction.
    #[test]
    fn one_message_log_refuses_reassigns_and_supersedes() {
        use toron_core::kernel::MachineState;

        let m = build_brief_machine();
        let mut ms = MachineState::at(BriefState::Idle, BriefContext::default());
        let mut cmds = Vec::new();
        let feed = |ms: &MachineState<BriefState, BriefContext>,
                    event: &BriefEvent,
                    cmds: &mut Vec<BriefCmd>| {
            let out = m.step_state(ms, event);
            cmds.extend(out.commands.clone());
            MachineState::at(
                out.configuration
                    .primary()
                    .cloned()
                    .unwrap_or(BriefState::Idle),
                out.context,
            )
        };
        ms = feed(&ms, &BriefEvent::Assign { pin: "A".into() }, &mut cmds);
        assert_eq!(ms.configuration.primary(), Some(&BriefState::Held));
        assert_eq!(ms.context.holder.as_deref(), Some("A"));

        ms = feed(&ms, &BriefEvent::Assign { pin: "B".into() }, &mut cmds);
        assert_eq!(ms.configuration.primary(), Some(&BriefState::Held));
        assert_eq!(
            ms.context.holder.as_deref(),
            Some("A"),
            "a held bead refuses"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, BriefCmd::Refuse { pin } if pin == "B")),
            "the refusal is a typed command: {cmds:?}"
        );

        ms = feed(&ms, &BriefEvent::Reassign { pin: "B".into() }, &mut cmds);
        assert_eq!(ms.context.holder.as_deref(), Some("B"));
        assert_eq!(ms.context.notified, vec!["A".to_string()]);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, BriefCmd::Notify { pin } if pin == "A")),
            "the replaced holder is notified: {cmds:?}"
        );

        ms = feed(&ms, &BriefEvent::Brief { id: "b1".into() }, &mut cmds);
        assert_eq!(ms.configuration.primary(), Some(&BriefState::Live));

        ms = feed(&ms, &BriefEvent::Brief { id: "b2".into() }, &mut cmds);
        assert_eq!(ms.configuration.primary(), Some(&BriefState::Live));
        assert_eq!(ms.context.live_brief.as_deref(), Some("b2"));
        assert_eq!(ms.context.withdrawn_briefs, vec!["b1".to_string()]);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, BriefCmd::Withdraw { id, pin } if id == "b1" && pin == "B")),
            "the older brief is withdrawn: {cmds:?}"
        );

        ms = feed(&ms, &BriefEvent::Done { id: "b2".into() }, &mut cmds);
        assert_eq!(ms.configuration.primary(), Some(&BriefState::Held));
        assert_eq!(ms.context.live_brief, None);
    }
}
