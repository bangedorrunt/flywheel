//! Bead-optional work unit statechart (bd-ihiqo.3).
//!
//! One work unit from unbound to closed: bind a bead id or an ad-hoc
//! objective plus its verify command, verify green, then close exactly one
//! way (ship land for a bead, commit plus journal receipt without one). A
//! handoff mints the bead, and only a handoff does. Pure fold; commands are
//! descriptive only. Mirrors `WorkUnit.tla` (event names match the spec
//! actions one for one so the conformance trace is a direct reading).

use serde::{Deserialize, Serialize};
use toron_core::kernel::{ActionQueue, Cmd as CmdTrait, Durability, Machine, StatechartDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum UnitState {
    Unbound,
    Bound,
    Verified,
    Closed,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitEvent {
    BindBead { id: String },
    BindAdHoc { objective: String },
    Verify,
    Handoff,
    CloseBead { id: String },
    CloseAdHoc,
    Retire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UnitContext {
    pub bead: Option<String>,
    pub objective: Option<String>,
    pub verified: bool,
    pub handed_off: bool,
    pub bead_created: bool,
    pub closed: Option<String>,
    pub closed_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitCmd {
    CloseBead { id: String },
    CloseCommit { key: String },
    CreateBead { objective: String },
    Journal { key: String },
}

impl CmdTrait for UnitCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct UnitProgramDef;

impl StatechartDefinition for UnitProgramDef {
    type State = UnitState;
    type Event = UnitEvent;
    type Context = UnitContext;
    type Cmd = UnitCmd;
    type Msg = UnitEvent;
    type Err = String;

    fn machine() -> Machine<Self::State, Self::Event, Self::Context, Self::Cmd> {
        Machine::builder(UnitState::Unbound, UnitContext::default())
            .state(UnitState::Unbound, |node| {
                let bind_bead = node
                    .transition(UnitState::Bound)
                    .on(|e| matches!(e, UnitEvent::BindBead { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let UnitEvent::BindBead { id } = e {
                            ctx.bead = Some(id.clone());
                            q.exec(UnitCmd::Journal {
                                key: format!("unit:bind:{id}"),
                            });
                        }
                    });
                let bind_adhoc = node
                    .transition(UnitState::Bound)
                    .on(|e| matches!(e, UnitEvent::BindAdHoc { .. }))
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let UnitEvent::BindAdHoc { objective } = e {
                            ctx.objective = Some(objective.clone());
                            q.exec(UnitCmd::Journal {
                                key: "unit:bind:adhoc".into(),
                            });
                        }
                    });
                let retire = node
                    .transition(UnitState::Retired)
                    .on(|e| matches!(e, UnitEvent::Retire));
                node.on(bind_bead).on(bind_adhoc).on(retire)
            })
            .state(UnitState::Bound, |node| {
                let verify = node
                    .transition(UnitState::Verified)
                    .on(|e| matches!(e, UnitEvent::Verify))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.verified = true;
                        q.exec(UnitCmd::Journal {
                            key: "unit:verify:green".into(),
                        });
                    });
                let handoff = node
                    .transition(UnitState::Bound)
                    .on(|e| matches!(e, UnitEvent::Handoff))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.handed_off = true;
                        if ctx.bead.is_none()
                            && !ctx.bead_created
                            && let Some(objective) = ctx.objective.clone()
                        {
                            ctx.bead_created = true;
                            q.exec(UnitCmd::CreateBead { objective });
                        }
                        q.exec(UnitCmd::Journal {
                            key: "unit:handoff".into(),
                        });
                    });
                let close_bead = node
                    .transition(UnitState::Closed)
                    .on(|e| matches!(e, UnitEvent::CloseBead { .. }))
                    .guard(|ctx, _| ctx.verified && ctx.bead.is_some() && ctx.closed.is_none())
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let UnitEvent::CloseBead { id } = e {
                            ctx.closed = Some("ship".into());
                            ctx.closed_count += 1;
                            q.exec(UnitCmd::CloseBead { id: id.clone() });
                        }
                    });
                let close_adhoc = node
                    .transition(UnitState::Closed)
                    .on(|e| matches!(e, UnitEvent::CloseAdHoc))
                    .guard(|ctx, _| ctx.verified && ctx.bead.is_none() && ctx.closed.is_none())
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.closed = Some("commit".into());
                        ctx.closed_count += 1;
                        q.exec(UnitCmd::CloseCommit {
                            key: "unit:commit".into(),
                        });
                    });
                let retire = node
                    .transition(UnitState::Retired)
                    .on(|e| matches!(e, UnitEvent::Retire));
                node.on(verify)
                    .on(handoff)
                    .on(close_bead)
                    .on(close_adhoc)
                    .on(retire)
            })
            .state(UnitState::Verified, |node| {
                let handoff = node
                    .transition(UnitState::Verified)
                    .on(|e| matches!(e, UnitEvent::Handoff))
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.handed_off = true;
                        if ctx.bead.is_none()
                            && !ctx.bead_created
                            && let Some(objective) = ctx.objective.clone()
                        {
                            ctx.bead_created = true;
                            q.exec(UnitCmd::CreateBead { objective });
                        }
                        q.exec(UnitCmd::Journal {
                            key: "unit:handoff".into(),
                        });
                    });
                let close_bead = node
                    .transition(UnitState::Closed)
                    .on(|e| matches!(e, UnitEvent::CloseBead { .. }))
                    .guard(|ctx, _| ctx.verified && ctx.bead.is_some() && ctx.closed.is_none())
                    .action(|ctx, e, q: &mut ActionQueue<_, _>| {
                        if let UnitEvent::CloseBead { id } = e {
                            ctx.closed = Some("ship".into());
                            ctx.closed_count += 1;
                            q.exec(UnitCmd::CloseBead { id: id.clone() });
                        }
                    });
                let close_adhoc = node
                    .transition(UnitState::Closed)
                    .on(|e| matches!(e, UnitEvent::CloseAdHoc))
                    .guard(|ctx, _| ctx.verified && ctx.bead.is_none() && ctx.closed.is_none())
                    .action(|ctx, _, q: &mut ActionQueue<_, _>| {
                        ctx.closed = Some("commit".into());
                        ctx.closed_count += 1;
                        q.exec(UnitCmd::CloseCommit {
                            key: "unit:commit".into(),
                        });
                    });
                let retire = node
                    .transition(UnitState::Retired)
                    .on(|e| matches!(e, UnitEvent::Retire));
                node.on(handoff).on(close_bead).on(close_adhoc).on(retire)
            })
            .state(UnitState::Closed, |node| {
                let retire = node
                    .transition(UnitState::Retired)
                    .on(|e| matches!(e, UnitEvent::Retire));
                node.on(retire)
            })
            .final_state(UnitState::Retired, |node| node)
            .build()
    }

    fn init_event() -> Self::Event {
        UnitEvent::Retire
    }

    fn extract_event(msg: &Self::Msg) -> Option<Self::Event> {
        Some(msg.clone())
    }

    fn failure(_failure: &toron_core::kernel::Failure<Self::Err>) -> Self::Msg {
        UnitEvent::Retire
    }
}

#[must_use]
pub fn build_work_unit_machine() -> Machine<UnitState, UnitEvent, UnitContext, UnitCmd> {
    UnitProgramDef::machine()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One message log per route: a bead closes by ship, an ad-hoc unit
    /// closes by commit, and a handoff mints the bead exactly once.
    #[test]
    fn one_message_log_per_close_route_plus_handoff() {
        use toron_core::kernel::MachineState;

        let m = build_work_unit_machine();
        let mut ms = MachineState::at(UnitState::Unbound, UnitContext::default());
        let mut cmds = Vec::new();
        let feed = |ms: &MachineState<UnitState, UnitContext>,
                    event: &UnitEvent,
                    cmds: &mut Vec<UnitCmd>| {
            let out = m.step_state(ms, event);
            cmds.extend(out.commands.clone());
            MachineState::at(
                out.configuration
                    .primary()
                    .cloned()
                    .unwrap_or(UnitState::Unbound),
                out.context,
            )
        };

        // Bead route: bind, verify, close by ship.
        ms = feed(&ms, &UnitEvent::BindBead { id: "bd-1".into() }, &mut cmds);
        ms = feed(&ms, &UnitEvent::Verify, &mut cmds);
        ms = feed(&ms, &UnitEvent::CloseBead { id: "bd-1".into() }, &mut cmds);
        assert_eq!(ms.configuration.primary(), Some(&UnitState::Closed));
        assert!(!ms.context.bead_created, "no handoff, no mint");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, UnitCmd::CloseBead { id } if id == "bd-1")),
            "a bead closes by ship land: {cmds:?}"
        );

        // Ad-hoc route with a handoff: the handoff mints exactly one bead,
        // and the close goes by commit.
        let mut ms = MachineState::at(UnitState::Unbound, UnitContext::default());
        ms = feed(
            &ms,
            &UnitEvent::BindAdHoc {
                objective: "o".into(),
            },
            &mut cmds,
        );
        let before: usize = cmds
            .iter()
            .filter(|c| matches!(c, UnitCmd::CreateBead { .. }))
            .count();
        ms = feed(&ms, &UnitEvent::Handoff, &mut cmds);
        ms = feed(&ms, &UnitEvent::Verify, &mut cmds);
        ms = feed(&ms, &UnitEvent::Handoff, &mut cmds);
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, UnitCmd::CreateBead { .. }))
                .count(),
            before + 1,
            "one handoff mints one bead, a second handoff mints none: {cmds:?}"
        );
        ms = feed(&ms, &UnitEvent::CloseAdHoc, &mut cmds);
        assert_eq!(ms.configuration.primary(), Some(&UnitState::Closed));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, UnitCmd::CloseCommit { .. }))
        );
    }

    /// An unverified close never fires: the guard holds the transition shut.
    #[test]
    fn an_unverified_close_does_not_fire() {
        use toron_core::kernel::MachineState;

        let m = build_work_unit_machine();
        let ms = MachineState::at(UnitState::Unbound, UnitContext::default());
        let mut cmds = Vec::new();
        let out = m.step_state(
            &ms,
            &UnitEvent::BindAdHoc {
                objective: "o".into(),
            },
        );
        let ms = MachineState::at(
            out.configuration
                .primary()
                .cloned()
                .unwrap_or(UnitState::Unbound),
            out.context,
        );
        cmds.extend(out.commands.clone());
        let before = ms.clone();
        let out = m.step_state(&ms, &UnitEvent::CloseAdHoc);
        assert_eq!(
            out.configuration.primary(),
            before.configuration.primary(),
            "no verify, no close"
        );
        assert!(
            cmds.iter()
                .all(|c| !matches!(c, UnitCmd::CloseCommit { .. })),
            "no close command without green verify: {cmds:?}"
        );
    }

    /// bd-ihiqo.3: the conformance trace is a real chart run — once with a
    /// bead (bind, verify, close by ship) and once ad-hoc (bind, handoff,
    /// verify, close by commit). With FLYWHEEL_EVERYSTATE set the test runs
    /// everystate conform against WorkUnit.tla itself.
    #[test]
    fn conformance_traces_match_the_model() {
        use toron_core::kernel::MachineState;

        fn run(events: &[UnitEvent]) -> Vec<serde_json::Value> {
            let m = build_work_unit_machine();
            let mut ms = MachineState::at(UnitState::Unbound, UnitContext::default());
            let mut trace = vec![serde_json::json!({
                "kind": "none", "verified": false, "handedOff": false,
                "beadCreated": false, "closed": "", "closeCount": 0,
            })];
            for e in events {
                let out = m.step_state(&ms, e);
                let name = match e {
                    UnitEvent::BindBead { .. } => "BindBead",
                    UnitEvent::BindAdHoc { .. } => "BindAdHoc",
                    UnitEvent::Verify => "Verify",
                    UnitEvent::Handoff => "Handoff",
                    UnitEvent::CloseBead { .. } => "CloseBead",
                    UnitEvent::CloseAdHoc => "CloseAdHoc",
                    UnitEvent::Retire => "Retire",
                };
                ms = MachineState::at(
                    out.configuration
                        .primary()
                        .cloned()
                        .unwrap_or(UnitState::Unbound),
                    out.context.clone(),
                );
                trace.push(serde_json::json!({
                    "action": name,
                    "kind": match ms.context.bead {
                        Some(_) => "bead",
                        None if ms.context.objective.is_some() => "adhoc",
                        None => "none",
                    },
                    "verified": ms.context.verified,
                    "handedOff": ms.context.handed_off,
                    "beadCreated": ms.context.bead_created,
                    "closed": ms.context.closed.clone().unwrap_or_default(),
                    "closeCount": ms.context.closed_count,
                }));
            }
            trace
        }
        fn to_jsonl(trace: &[serde_json::Value]) -> String {
            trace
                .iter()
                .map(|o| serde_json::to_string(o).expect("trace serializes"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        }
        let bead_run = run(&[
            UnitEvent::BindBead { id: "bd-1".into() },
            UnitEvent::Verify,
            UnitEvent::CloseBead { id: "bd-1".into() },
        ]);
        assert_eq!(
            bead_run
                .last()
                .and_then(|o| o.get("action"))
                .and_then(|a| a.as_str()),
            Some("CloseBead")
        );
        let adhoc_run = run(&[
            UnitEvent::BindAdHoc {
                objective: "o".into(),
            },
            UnitEvent::Handoff,
            UnitEvent::Verify,
            UnitEvent::CloseAdHoc,
        ]);
        assert_eq!(
            adhoc_run
                .last()
                .and_then(|o| o.get("closed"))
                .and_then(|c| c.as_str()),
            Some("commit")
        );
        let mut bad = bead_run.clone();
        bad.push(serde_json::json!({"action": "CloseBead"}));
        let out_path = |var: &str, fallback: &str| {
            std::env::var(var)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::env::temp_dir().join(fallback))
        };
        let check_in = |var: &str, fallback: &str, want: &str, fixture: &str| {
            let path = out_path(var, fallback);
            if std::env::var(var).is_err() && std::env::var("FLYWHEEL_EVERYSTATE").is_err() {
                let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(".agents/skills/verify-protocol-tla/traces");
                assert_eq!(
                    std::fs::read_to_string(root.join(fixture)).expect("checked-in trace"),
                    want,
                    "the recorded trace drifted from the real run; regenerate the fixture"
                );
            }
            std::fs::write(&path, want).expect("write trace");
            path
        };
        let good_path = check_in(
            "CONFORM_TRACE_OUT",
            "wu-recorded.jsonl",
            &to_jsonl(&bead_run),
            "WorkUnit.recorded.jsonl",
        );
        let adhoc_path = check_in(
            "CONFORM_ADHOC_TRACE_OUT",
            "wu-adhoc.jsonl",
            &to_jsonl(&adhoc_run),
            "WorkUnit.adhoc.recorded.jsonl",
        );
        let bad_path = check_in(
            "CONFORM_BAD_TRACE_OUT",
            "wu-double-close.jsonl",
            &to_jsonl(&bad),
            "WorkUnit.double-close.jsonl",
        );
        let Some(checker) = std::env::var("FLYWHEEL_EVERYSTATE").ok() else {
            return;
        };
        let models = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".agents/skills/verify-protocol-tla/models");
        let run = |trace: &std::path::Path| {
            std::process::Command::new(&checker)
                .arg("conform")
                .arg("-trace")
                .arg(trace)
                .arg("-config")
                .arg(models.join("WorkUnit.cfg"))
                .arg(models.join("WorkUnit.tla"))
                .output()
                .expect("everystate conform")
        };
        for good in [&good_path, &adhoc_path] {
            let out = run(good);
            assert!(
                out.status.success(),
                "recorded trace must conform: {}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
        let bad_out = run(&bad_path);
        assert_eq!(bad_out.status.code(), Some(20), "a double close must fail");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&bad_out.stdout),
            String::from_utf8_lossy(&bad_out.stderr)
        );
        assert!(
            text.contains("CloseBead"),
            "the rejection names the step: {text}"
        );
    }
}
