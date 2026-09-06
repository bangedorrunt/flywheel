// governed-by: ADR-0025
//! One ingress seam (ADR-0025 Spec 6): herdr events, toron watch hits, YAML
//! workflow triggers, and cron ticks all arrive as one [`IngressEvent`] and
//! are mapped into Spec 3's [`crate::durable::LoopMsg`] at the
//! [`handle_ingress`] boundary (bd-0or2). Two enums, one seam — no new bus.
//! Adapters are injected closures; this module does no IO. The runtime wires
//! the real leaves (herdr subscribe, doorbell, WorkflowHost notify, cron
//! runner) at the call sites. Zero behavior change: `tick` + journal remain
//! the pins.
use serde::{Deserialize, Serialize};
use toron_workflow::schema::Trigger;

/// Minimal typed herdr event surface the loop consumes today. Pane death is
/// observable — the reclaim trigger (ADR-0025 Spec 1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HerdrEvent {
    /// A pane's agent status changed (idle/working/blocked/unknown).
    PaneAgentStatusChanged { pane_id: String, status: String },
    /// A pane exited: process gone.
    PaneExited { pane_id: String, herdr_name: String },
}

/// Minimal typed watch hit (the `toron watch --once` doorbell).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchHit {
    pub project: String,
    pub kind: String,
    #[serde(default)]
    pub event_id: Option<String>,
}

/// Ledger fact event kind (bd-cy0s).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LedgerEvent {
    Claimed,
    Closed,
    Reopened,
    Blocked,
}

/// The one ingress type: Jido's sensor shape onto the spine we already run
/// (ADR-0025 Spec 6). Unification is the enum — no new bus. Adjacent
/// tagging so the inner `HerdrEvent` type tag never collides.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ingress", content = "payload", rename_all = "snake_case")]
pub enum IngressEvent {
    Herdr(HerdrEvent),
    Watch(WatchHit),
    WorkflowTrigger(Trigger),
    CronTick {
        name: String,
    },
    LedgerFact {
        bead_id: String,
        event: LedgerEvent,
    },
    PaneStalled {
        pane_id: String,
        herdr_name: String,
        last_status: String,
        bead_id: Option<String>,
        #[serde(default = "default_stall_reason")]
        reason: String,
        #[serde(default = "default_evidence_tail")]
        evidence_tail: String,
        #[serde(default = "default_stall_severity")]
        severity: String,
    },
    LeaseLapsed {
        path: String,
        bead_id: Option<String>,
    },
    GitFact {
        repo: String,
        sha: String,
        subject: String,
        bead_id: Option<String>,
    },
}

fn default_stall_reason() -> String {
    "silent_idle".to_string()
}
fn default_evidence_tail() -> String {
    String::new()
}
fn default_stall_severity() -> String {
    "stalled".to_string()
}

/// Human-readable summary for a fact — the `[doorbell] <fact-summary>` line
/// the captain pane receives (bd-cy0s captain surface). Pure, unit-tested.
pub fn fact_summary(event: &IngressEvent) -> Option<String> {
    match event {
        IngressEvent::LedgerFact { bead_id, event } => {
            let ev = match event {
                LedgerEvent::Claimed => "claimed",
                LedgerEvent::Closed => "closed",
                LedgerEvent::Reopened => "reopened",
                LedgerEvent::Blocked => "blocked",
            };
            Some(format!("ledger {bead_id} {ev}"))
        }
        IngressEvent::PaneStalled {
            pane_id,
            herdr_name,
            last_status,
            bead_id,
            reason,
            severity,
            ..
        } => {
            let bead = bead_id
                .as_deref()
                .map(|b| format!(" bead {b}"))
                .unwrap_or_default();
            Some(format!(
                "pane stalled {pane_id} ({herdr_name} {last_status} {reason}/{severity}){bead}"
            ))
        }
        IngressEvent::LeaseLapsed { path, bead_id } => {
            let bead = bead_id
                .as_deref()
                .map(|b| format!(" bead {b}"))
                .unwrap_or_default();
            Some(format!("lease lapsed {path}{bead}"))
        }
        IngressEvent::GitFact {
            repo,
            sha,
            subject,
            bead_id,
        } => {
            let bead = bead_id
                .as_deref()
                .map(|b| format!(" bead {b}"))
                .unwrap_or_default();
            let short = if sha.len() > 7 { &sha[..7] } else { sha };
            Some(format!("git {repo} {short} {subject}{bead}"))
        }
        _ => None,
    }
}

/// Doorbell line for a fact — reuse existing wake path.
pub fn fact_doorbell_line(event: &IngressEvent) -> Option<String> {
    fact_summary(event).map(|s| format!("[doorbell] {s}"))
}

pub fn is_fact(event: &IngressEvent) -> bool {
    fact_summary(event).is_some()
}

/// Adapter sinks [`handle_ingress`] fans out to. Injected, never global.
pub struct IngressAdapters {
    pub herdr: Box<dyn Fn(&HerdrEvent) + Send + Sync>,
    pub watch: Box<dyn Fn(&WatchHit) + Send + Sync>,
    pub workflow: Box<dyn Fn(&Trigger) + Send + Sync>,
    pub cron: Box<dyn Fn(&str) + Send + Sync>,
    pub fact: Box<dyn Fn(&IngressEvent) + Send + Sync>,
}

impl Default for IngressAdapters {
    fn default() -> Self {
        Self {
            herdr: Box::new(|ev| {
                eprintln!("ingress miss (no handler): herdr {:?}", ev);
            }),
            watch: Box::new(|ev| {
                eprintln!("ingress miss (no handler): watch {:?}", ev);
            }),
            workflow: Box::new(|ev| {
                eprintln!("ingress miss (no handler): workflow {:?}", ev);
            }),
            cron: Box::new(|ev| {
                eprintln!("ingress miss (no handler): cron {:?}", ev);
            }),
            fact: Box::new(|_| {}),
        }
    }
}

/// bd-0or2 seam: map the unified [`IngressEvent`] bus into the typed
/// [`crate::durable::LoopMsg`] boundary (ADR-0025 Spec 6 deviation noted in
/// AGENTS.md 2026-08-30). Zero behavior change — `tick` + journal stay the
/// reducer pins; this is the typed conversion at the `handle_ingress` seam.
pub fn ingress_to_loop_msg(event: &IngressEvent) -> Option<crate::durable::LoopMsg> {
    match event {
        IngressEvent::Herdr(he) => {
            // event_type strings MUST match what apply_events_to_states
            // consumes ("pane.agent_status_changed" / "pane.exited") —
            // an invented string here is a silent no-op downstream.
            let pe = match he {
                HerdrEvent::PaneAgentStatusChanged { pane_id, status } => {
                    crate::durable::PollEvent {
                        event_type: "pane.agent_status_changed".into(),
                        pane_id: pane_id.clone(),
                        agent: None,
                        status: Some(status.clone()),
                        seq: 0,
                    }
                }
                HerdrEvent::PaneExited {
                    pane_id,
                    herdr_name,
                } => crate::durable::PollEvent {
                    event_type: "pane.exited".into(),
                    pane_id: pane_id.clone(),
                    agent: Some(herdr_name.clone()),
                    status: None,
                    seq: 0,
                },
            };
            Some(crate::durable::LoopMsg::Herdr(pe))
        }
        // Only a CLOSED ledger fact is a BrClose. Claimed/Blocked/Reopened
        // are NOT closes — mapping them here would corrupt per_bead_phase.
        IngressEvent::LedgerFact {
            bead_id,
            event: LedgerEvent::Closed,
        } => Some(crate::durable::LoopMsg::BrClose {
            bead_id: bead_id.clone(),
        }),
        // Watch/Workflow/Cron/other facts map is intentionally None today —
        // they fan into doorbell or fact journal, not LoopMsg. The seam
        // exists so a future tick arm can be added without a second bus.
        _ => None,
    }
}

/// One router: every external fact enters the system here. A `pane_exited`
/// and a `message_posted` trigger cannot bypass it — they ARE this enum.
pub fn handle_ingress(event: &IngressEvent, adapters: &IngressAdapters) {
    match event {
        IngressEvent::Herdr(e) => (adapters.herdr)(e),
        IngressEvent::Watch(w) => (adapters.watch)(w),
        IngressEvent::WorkflowTrigger(t) => (adapters.workflow)(t),
        IngressEvent::CronTick { name } => (adapters.cron)(name),
        IngressEvent::LedgerFact { .. }
        | IngressEvent::PaneStalled { .. }
        | IngressEvent::LeaseLapsed { .. }
        | IngressEvent::GitFact { .. } => (adapters.fact)(event),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Hits = Arc<Mutex<Vec<String>>>;

    /// §9 Wave 5: both a synthetic pane_exited and a message_posted trigger
    /// flow through handle_ingress — no bypass.
    #[test]
    fn ingress_pane_exited_and_message_posted_route_through_handle_ingress() {
        let herdr_hits: Hits = Arc::new(Mutex::new(Vec::new()));
        let wf_hits: Hits = Arc::new(Mutex::new(Vec::new()));
        let cron_hits: Hits = Arc::new(Mutex::new(Vec::new()));
        let watch_hits: Hits = Arc::new(Mutex::new(Vec::new()));

        let adapters = IngressAdapters {
            herdr: {
                let hits = herdr_hits.clone();
                Box::new(move |e: &HerdrEvent| {
                    if let HerdrEvent::PaneExited { herdr_name, .. } = e {
                        hits.lock().unwrap().push(herdr_name.clone());
                    }
                })
            },
            watch: {
                let hits = watch_hits.clone();
                Box::new(move |_w: &WatchHit| {
                    hits.lock().unwrap().push("watch".into());
                })
            },
            workflow: {
                let hits = wf_hits.clone();
                Box::new(move |_t: &Trigger| {
                    hits.lock().unwrap().push("trigger".into());
                })
            },
            cron: {
                let hits = cron_hits.clone();
                Box::new(move |name: &str| {
                    hits.lock().unwrap().push(name.to_string());
                })
            },
            fact: Box::new(|_| {}),
        };

        let exited = IngressEvent::Herdr(HerdrEvent::PaneExited {
            pane_id: "w1:p1".into(),
            herdr_name: "flywheel-flywheel-oc3".into(),
        });
        handle_ingress(&exited, &adapters);
        let posted = IngressEvent::WorkflowTrigger(Trigger::MessagePosted {
            channel: Some("crew".into()),
            project: Some("toron".into()),
        });
        handle_ingress(&posted, &adapters);
        handle_ingress(
            &IngressEvent::CronTick {
                name: "weekly".into(),
            },
            &adapters,
        );
        handle_ingress(
            &IngressEvent::Watch(WatchHit {
                project: "flywheel".into(),
                kind: "message_posted".into(),
                event_id: Some("abc".into()),
            }),
            &adapters,
        );

        assert_eq!(herdr_hits.lock().unwrap().len(), 1, "pane_exited routed");
        assert_eq!(wf_hits.lock().unwrap().len(), 1, "trigger routed");
        assert_eq!(*cron_hits.lock().unwrap(), vec!["weekly".to_string()]);
        assert_eq!(watch_hits.lock().unwrap().len(), 1, "watch routed");
    }

    #[test]
    fn ingress_serde_round_trip_all_variants() {
        let events = vec![
            IngressEvent::Herdr(HerdrEvent::PaneAgentStatusChanged {
                pane_id: "w8E:p2".into(),
                status: "working".into(),
            }),
            IngressEvent::Herdr(HerdrEvent::PaneExited {
                pane_id: "w8E:p3".into(),
                herdr_name: "GoldCove".into(),
            }),
            IngressEvent::Watch(WatchHit {
                project: "toron".into(),
                kind: "job".into(),
                event_id: None,
            }),
            IngressEvent::WorkflowTrigger(Trigger::MessagePosted {
                channel: None,
                project: None,
            }),
            IngressEvent::CronTick {
                name: "digest".into(),
            },
            IngressEvent::LedgerFact {
                bead_id: "bd-123".into(),
                event: LedgerEvent::Closed,
            },
            IngressEvent::PaneStalled {
                pane_id: "w1:p1".into(),
                herdr_name: "oc".into(),
                last_status: "idle".into(),
                bead_id: Some("bd-123".into()),
                reason: "silent_idle".into(),
                evidence_tail: String::new(),
                severity: "stalled".into(),
            },
            IngressEvent::LeaseLapsed {
                path: "src/main.rs".into(),
                bead_id: Some("bd-123".into()),
            },
            IngressEvent::GitFact {
                repo: "flywheel".into(),
                sha: "abc123def".into(),
                subject: "fix".into(),
                bead_id: None,
            },
        ];
        for e in events {
            let json = serde_json::to_string(&e).expect("serialize");
            let back: IngressEvent = serde_json::from_str(&json).expect("deserialize");
            let back_json = serde_json::to_string(&back).expect("serialize back");
            assert_eq!(back_json, json, "round trip lossless for {json}");
        }
    }

    #[test]
    fn facts_variants_route_through_handle_ingress() {
        let fact_hits: Hits = Arc::new(Mutex::new(Vec::new()));
        let adapters = IngressAdapters {
            herdr: Box::new(|_| {}),
            watch: Box::new(|_| {}),
            workflow: Box::new(|_| {}),
            cron: Box::new(|_| {}),
            fact: {
                let hits = fact_hits.clone();
                Box::new(move |e: &IngressEvent| {
                    if let Some(s) = fact_summary(e) {
                        hits.lock().unwrap().push(s);
                    }
                })
            },
        };
        for ev in [
            IngressEvent::LedgerFact {
                bead_id: "bd-1".into(),
                event: LedgerEvent::Claimed,
            },
            IngressEvent::PaneStalled {
                pane_id: "w1:p1".into(),
                herdr_name: "oc".into(),
                last_status: "idle".into(),
                bead_id: None,
                reason: "silent_idle".into(),
                evidence_tail: String::new(),
                severity: "stalled".into(),
            },
            IngressEvent::LeaseLapsed {
                path: "src/durable.rs".into(),
                bead_id: Some("bd-1".into()),
            },
            IngressEvent::GitFact {
                repo: "flywheel".into(),
                sha: "deadbeefcafe".into(),
                subject: "commit".into(),
                bead_id: Some("bd-1".into()),
            },
        ] {
            handle_ingress(&ev, &adapters);
        }
        let hits = fact_hits.lock().unwrap();
        assert_eq!(
            hits.len(),
            4,
            "all fact variants routed through handle_ingress"
        );
        assert!(hits[0].contains("ledger bd-1 claimed"));
        assert!(hits[1].contains("pane stalled"));
        assert!(hits[2].contains("lease lapsed"));
        assert!(hits[3].contains("git flywheel"));
    }

    #[test]
    fn facts_captain_surface_renders_summaries() {
        let ledger = IngressEvent::LedgerFact {
            bead_id: "bd-9".into(),
            event: LedgerEvent::Closed,
        };
        assert_eq!(fact_summary(&ledger).unwrap(), "ledger bd-9 closed");
        assert_eq!(
            fact_doorbell_line(&ledger).unwrap(),
            "[doorbell] ledger bd-9 closed"
        );
        let stalled = IngressEvent::PaneStalled {
            pane_id: "w1:p1".into(),
            herdr_name: "oc".into(),
            last_status: "idle".into(),
            bead_id: Some("bd-9".into()),
            reason: "silent_idle".into(),
            evidence_tail: String::new(),
            severity: "stalled".into(),
        };
        assert!(
            fact_summary(&stalled)
                .unwrap()
                .contains("pane stalled w1:p1")
        );
        assert!(
            fact_doorbell_line(&stalled)
                .unwrap()
                .starts_with("[doorbell] pane stalled")
        );
        let lapsed = IngressEvent::LeaseLapsed {
            path: "src/main.rs".into(),
            bead_id: None,
        };
        assert_eq!(fact_summary(&lapsed).unwrap(), "lease lapsed src/main.rs");
        assert_eq!(
            fact_doorbell_line(&lapsed).unwrap(),
            "[doorbell] lease lapsed src/main.rs"
        );
        let git = IngressEvent::GitFact {
            repo: "flywheel".into(),
            sha: "abc123def456".into(),
            subject: "fix guard".into(),
            bead_id: None,
        };
        assert!(fact_summary(&git).unwrap().contains("git flywheel abc123d"));
        assert!(
            fact_doorbell_line(&git)
                .unwrap()
                .starts_with("[doorbell] git")
        );
        // non-fact returns None
        let herdr = IngressEvent::Herdr(HerdrEvent::PaneExited {
            pane_id: "w1:p1".into(),
            herdr_name: "oc".into(),
        });
        assert!(fact_summary(&herdr).is_none());
        assert!(fact_doorbell_line(&herdr).is_none());
    }

    #[test]
    fn facts_serde_adjacent_tag_style_consistent() {
        let ev = IngressEvent::LedgerFact {
            bead_id: "bd-xyz".into(),
            event: LedgerEvent::Blocked,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ingress"], "ledger_fact");
        assert!(v["payload"]["bead_id"].is_string());
        assert_eq!(v["payload"]["event"], "blocked");
        let back: IngressEvent = serde_json::from_str(&json).unwrap();
        let back_json = serde_json::to_string(&back).unwrap();
        assert_eq!(back_json, json);
    }

    #[test]
    fn bd0or2_ingress_to_loop_msg_seam_exists_and_maps_herdr() {
        // event_type strings MUST match apply_events_to_states consumers —
        // an invented string would be a silent no-op downstream.
        let status = IngressEvent::Herdr(HerdrEvent::PaneAgentStatusChanged {
            pane_id: "w1:p1".into(),
            status: "idle".into(),
        });
        match ingress_to_loop_msg(&status).expect("herdr must map") {
            crate::durable::LoopMsg::Herdr(pe) => {
                assert_eq!(pe.event_type, "pane.agent_status_changed");
                assert_eq!(pe.status.as_deref(), Some("idle"));
            }
            other => panic!("expected LoopMsg::Herdr, got {other:?}"),
        }
        let he = IngressEvent::Herdr(HerdrEvent::PaneExited {
            pane_id: "w1:p1".into(),
            herdr_name: "oc".into(),
        });
        match ingress_to_loop_msg(&he).expect("herdr must map") {
            crate::durable::LoopMsg::Herdr(pe) => {
                assert_eq!(pe.event_type, "pane.exited");
            }
            other => panic!("expected LoopMsg::Herdr, got {other:?}"),
        }
        let lf = IngressEvent::LedgerFact {
            bead_id: "bd-123".into(),
            event: LedgerEvent::Closed,
        };
        let lm2 = ingress_to_loop_msg(&lf).expect("closed ledger fact maps to BrClose");
        assert!(matches!(lm2, crate::durable::LoopMsg::BrClose { .. }));
        // A claim is NOT a close — must not corrupt per_bead_phase.
        let claim = IngressEvent::LedgerFact {
            bead_id: "bd-123".into(),
            event: LedgerEvent::Claimed,
        };
        assert!(
            ingress_to_loop_msg(&claim).is_none(),
            "claimed fact must not map to BrClose"
        );
        let watch = IngressEvent::Watch(WatchHit {
            project: "flywheel".into(),
            kind: "9".into(),
            event_id: None,
        });
        assert!(
            ingress_to_loop_msg(&watch).is_none(),
            "watch stays on doorbell, not loop"
        );
    }

    #[test]
    fn bd0or2_herdr_event_folds_into_tick_pending_and_drains_on_next_tick() {
        // bd-55nx.11: exercise the LIVE entry (workspace tick), not the
        // slice reducer — a green slice with a broken live entry must fail.
        use crate::durable::{LoopModel, PollEvent};
        use crate::workspace::{Msg as WsMsg, WorkspaceModel, tick as ws_tick};
        let ws = WorkspaceModel {
            swarm: LoopModel::default(),
            ..Default::default()
        };
        let pe = PollEvent {
            event_type: "pane.agent_status_changed".into(),
            pane_id: "w1:p1".into(),
            agent: Some("a".into()),
            status: Some("idle".into()),
            seq: 1,
        };
        let (next, cmds) = ws_tick(&ws, &WsMsg::Herdr { event: pe });
        assert!(cmds.is_empty());
        assert_eq!(
            next.swarm.pending_events.len(),
            1,
            "Herdr buffers into model'"
        );
        let (next2, _) = ws_tick(&next, &WsMsg::Tick { now: 0 });
        assert!(
            next2.swarm.pending_events.is_empty(),
            "Tick drains buffered events"
        );
    }

    #[test]
    fn ingress_tests_use_live_entry_not_slice() {
        // bd-55nx.11 RED: tests must exercise the live entry
        // (workspace tick), never the slice reducer directly.
        // (needle split so this self-scan does not match its own source)
        let needle = ["durable", "::tick"].concat();
        let src = include_str!("ingress.rs");
        assert!(
            !src.contains(&needle),
            "RED (55nx.11): ingress.rs still calls the slice reducer; port tests to the live entry"
        );
    }
}
