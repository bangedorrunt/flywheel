//! Lease waitlist statechart (bd-u1n91).
//!
//! One path from free to retired. A blocked acquire queues the pin and emits
//! ONE typed request-release; any release hands the path to the head waiter.
//! Pure fold; commands are descriptive only. Mirrors `LeaseWaitlist.tla`.

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LeaseState {
    Free,
    Held,
    Waiting,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseEvent {
    Acquire { pin: String },
    Release { source: String },
    Expire,
    Withdraw { pin: String },
    Retire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LeaseContext {
    pub holder: Option<String>,
    pub queue: Vec<String>,
    pub notified: Vec<String>,
    pub served: Vec<String>,
    pub last_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseCmd {
    RequestRelease { pin: String },
    Grant { pin: String, source: String },
    Journal { key: String },
}

impl CmdTrait for LeaseCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct LeaseWaitlistProgramDef;

fn handoff(ctx: &mut LeaseContext, source: &str, q: &mut ActionQueue<LeaseCmd, LeaseEvent>) {
    ctx.last_source = Some(source.to_string());
    if let Some(next) = ctx.queue.first().cloned() {
        ctx.queue.remove(0);
        ctx.served.push(next.clone());
        ctx.holder = Some(next.clone());
        q.exec(LeaseCmd::Grant {
            pin: next,
            source: source.to_string(),
        });
        q.exec(LeaseCmd::Journal {
            key: format!("lease:handoff:{source}"),
        });
    } else {
        ctx.holder = None;
        q.exec(LeaseCmd::Journal {
            key: format!("lease:release:{source}"),
        });
    }
}

impl StatechartDefinition for LeaseWaitlistProgramDef {
    type State = LeaseState;
    type Event = LeaseEvent;
    type Context = LeaseContext;
    type Cmd = LeaseCmd;
    type Msg = LeaseEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(LeaseState::Free, LeaseContext::default())
            .state(LeaseState::Free, |node| {
                let acquire = node
                    .transition(LeaseState::Held)
                    .on(|e| matches!(e, LeaseEvent::Acquire { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let LeaseEvent::Acquire { pin } = e {
                            ctx.holder = Some(pin.clone());
                            q.exec(LeaseCmd::Journal {
                                key: format!("lease:grant:{pin}"),
                            });
                        }
                    });
                let retire = node
                    .transition(LeaseState::Retired)
                    .on(|e| matches!(e, LeaseEvent::Retire));
                node.on(acquire).on(retire)
            })
            .state(LeaseState::Held, |node| {
                let queue = node
                    .transition(LeaseState::Waiting)
                    .on(|e| matches!(e, LeaseEvent::Acquire { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let LeaseEvent::Acquire { pin } = e {
                            ctx.queue.push(pin.clone());
                            ctx.notified.push(pin.clone());
                            q.exec(LeaseCmd::RequestRelease { pin: pin.clone() });
                            q.exec(LeaseCmd::Journal {
                                key: format!("lease:queued:{pin}"),
                            });
                        }
                    });
                let release = node
                    .transition(LeaseState::Free)
                    .on(|e| matches!(e, LeaseEvent::Release { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let LeaseEvent::Release { source } = e {
                            handoff(ctx, source, q);
                        }
                    });
                let expire = node
                    .transition(LeaseState::Free)
                    .on(|e| matches!(e, LeaseEvent::Expire))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| handoff(ctx, "ttl", q));
                let retire = node
                    .transition(LeaseState::Retired)
                    .on(|e| matches!(e, LeaseEvent::Retire));
                node.on(queue).on(release).on(expire).on(retire)
            })
            .state(LeaseState::Waiting, |node| {
                let queue = node
                    .transition(LeaseState::Waiting)
                    .on(|e| matches!(e, LeaseEvent::Acquire { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let LeaseEvent::Acquire { pin } = e {
                            ctx.queue.push(pin.clone());
                            ctx.notified.push(pin.clone());
                            q.exec(LeaseCmd::RequestRelease { pin: pin.clone() });
                            q.exec(LeaseCmd::Journal {
                                key: format!("lease:queued:{pin}"),
                            });
                        }
                    });
                let release = node
                    .transition(LeaseState::Waiting)
                    .on(|e| matches!(e, LeaseEvent::Release { .. }))
                    .guard(|ctx, _| ctx.queue.len() > 1)
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let LeaseEvent::Release { source } = e {
                            handoff(ctx, source, q);
                        }
                    });
                let release_last = node
                    .transition(LeaseState::Held)
                    .on(|e| matches!(e, LeaseEvent::Release { .. }))
                    .guard(|ctx, _| ctx.queue.len() <= 1)
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let LeaseEvent::Release { source } = e {
                            handoff(ctx, source, q);
                        }
                    });
                let expire = node
                    .transition(LeaseState::Held)
                    .on(|e| matches!(e, LeaseEvent::Expire))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| handoff(ctx, "ttl", q));
                let withdraw = node
                    .transition(LeaseState::Held)
                    .on(|e| matches!(e, LeaseEvent::Withdraw { .. }))
                    .guard(|ctx, _| ctx.queue.len() <= 1)
                    .action(|ctx, e, _q: &mut ActionQueue<_, _>| {
                        if let LeaseEvent::Withdraw { pin } = e {
                            ctx.queue.retain(|p| p != pin);
                        }
                    });
                let retire = node
                    .transition(LeaseState::Retired)
                    .on(|e| matches!(e, LeaseEvent::Retire));
                node.on(queue)
                    .on(release)
                    .on(release_last)
                    .on(expire)
                    .on(withdraw)
                    .on(retire)
            })
            .final_state(LeaseState::Retired, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        LeaseEvent::Retire
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        LeaseEvent::Retire
    }
}

#[must_use]
pub fn build_lease_waitlist_machine() -> Machine<LeaseState, LeaseEvent, LeaseContext, LeaseCmd> {
    LeaseWaitlistProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure projection of one statechart step into `LeaseWaitlist.tla`
    /// variable names for `everystate conform`. No clock, socket or env
    /// here — the driver test owns the file write.
    #[must_use]
    fn conform_step(event: &LeaseEvent, ctx: &LeaseContext) -> serde_json::Value {
        let (action, params) = match event {
            LeaseEvent::Acquire { pin } => (
                if ctx.notified.contains(pin) {
                    "Queue"
                } else {
                    "AcquireFree"
                },
                vec![pin.clone()],
            ),
            LeaseEvent::Release { source } => ("Release", vec![source.clone()]),
            LeaseEvent::Expire => ("Release", vec!["ttl".to_string()]),
            LeaseEvent::Withdraw { pin } => ("Withdraw", vec![pin.clone()]),
            LeaseEvent::Retire => ("Retire", vec![]),
        };
        let pad = |v: &Vec<String>| -> Vec<String> {
            let mut out = v.clone();
            while out.len() < 2 {
                out.push("none".into());
            }
            out
        };
        serde_json::json!({
            "action": action,
            "params": params,
            "holder": ctx.holder.iter().cloned().collect::<Vec<_>>(),
            "arr": pad(&ctx.notified),
            "arrLen": ctx.notified.len(),
            "srv": pad(&ctx.served),
            "srvLen": ctx.served.len(),
            "queued": ctx.queue,
            "arrived": ctx.notified,
            "notified": ctx.notified,
            "badRelease": [],
        })
    }

    #[test]
    fn one_message_log_hands_off_fifo_and_accounts_for_every_waiter() {
        use toron_core::kernel::MachineState;

        let m = build_lease_waitlist_machine();
        let mut ms = MachineState::at(LeaseState::Free, LeaseContext::default());
        let mut cmds = Vec::new();
        for event in [
            LeaseEvent::Acquire { pin: "A".into() },
            LeaseEvent::Acquire { pin: "B".into() },
            LeaseEvent::Acquire { pin: "C".into() },
        ] {
            let out = m.step_state(&ms, &event);
            cmds.extend(out.commands.clone());
            ms = MachineState::at(
                out.configuration
                    .primary()
                    .cloned()
                    .unwrap_or(LeaseState::Free),
                out.context,
            );
        }
        assert_eq!(ms.configuration.primary(), Some(&LeaseState::Waiting));
        assert_eq!(ms.context.queue, vec!["B".to_string(), "C".to_string()]);
        assert_eq!(
            ms.context.notified,
            vec!["B".to_string(), "C".to_string()],
            "one typed request-release per waiter"
        );
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, LeaseCmd::RequestRelease { .. }))
                .count(),
            2
        );

        let out = m.step_state(
            &ms,
            &LeaseEvent::Release {
                source: "commit".into(),
            },
        );
        assert!(
            out.commands.iter().any(|c| matches!(
                c,
                LeaseCmd::Grant { pin, source }
                    if pin == "B" && source == "commit"
            )),
            "the release hands over to the FIFO head: {:?}",
            out.commands
        );
        ms = MachineState::at(
            out.configuration
                .primary()
                .cloned()
                .unwrap_or(LeaseState::Free),
            out.context,
        );
        assert_eq!(ms.context.holder.as_deref(), Some("B"));
        assert_eq!(ms.context.queue, vec!["C".to_string()]);

        let out = m.step_state(&ms, &LeaseEvent::Expire);
        ms = MachineState::at(
            out.configuration
                .primary()
                .cloned()
                .unwrap_or(LeaseState::Free),
            out.context,
        );
        assert_eq!(ms.context.holder.as_deref(), Some("C"), "TTL hands over");
        assert!(ms.context.queue.is_empty());
        assert_eq!(ms.context.last_source.as_deref(), Some("ttl"));
    }

    /// Records a real statechart message log as an `everystate conform` trace.
    /// Set CONFORM_TRACE_OUT to regenerate the fixture; with no env this is a
    /// pure self-check of the projection, never an ambient-state read.
    #[test]
    fn conform_trace_projection_is_deterministic() {
        use toron_core::kernel::MachineState;

        let m = build_lease_waitlist_machine();
        let mut ms = MachineState::at(LeaseState::Free, LeaseContext::default());
        let mut rows = vec![serde_json::json!({
            "holder": Vec::<String>::new(),
            "arr": ["none", "none"],
            "arrLen": 0,
            "srv": ["none", "none"],
            "srvLen": 0,
            "queued": Vec::<String>::new(),
            "arrived": Vec::<String>::new(),
            "notified": Vec::<String>::new(),
            "badRelease": Vec::<String>::new(),
        })];
        for event in [
            LeaseEvent::Acquire { pin: "A".into() },
            LeaseEvent::Acquire { pin: "B".into() },
            LeaseEvent::Acquire { pin: "C".into() },
            LeaseEvent::Release {
                source: "commit".into(),
            },
            LeaseEvent::Expire,
        ] {
            let out = m.step_state(&ms, &event);
            ms = MachineState::at(
                out.configuration
                    .primary()
                    .cloned()
                    .unwrap_or(LeaseState::Free),
                out.context.clone(),
            );
            rows.push(conform_step(&event, &ms.context));
        }
        assert_eq!(rows.len(), 6, "one initial state plus five real steps");
        assert_eq!(
            rows.iter()
                .filter_map(|r| r["action"].as_str())
                .collect::<Vec<_>>(),
            vec!["AcquireFree", "Queue", "Queue", "Release", "Release"]
        );
        if let Ok(path) = std::env::var("CONFORM_TRACE_OUT") {
            let text = rows
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(path, format!("{text}\n")).expect("write conform trace");
        }
    }
}
