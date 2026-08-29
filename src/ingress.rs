// governed-by: ADR-0025
//! One ingress seam (ADR-0025 Spec 6): herdr events, toron watch hits, YAML
//! workflow triggers, and cron ticks all arrive as one [`IngressEvent`] —
//! the same Msg type as Spec 3's `LoopMsg` — and flow through
//! [`handle_ingress`]. Adapters are injected closures; this module does no
//! IO. The runtime wires the real leaves (herdr subscribe, doorbell,
//! WorkflowHost notify, cron runner) at the call sites.

use serde::{Deserialize, Serialize};
use toron_workflow::schema::Trigger;

/// Minimal typed herdr event surface the loop consumes today. Pane death is
/// observable — the reclaim trigger (ADR-0025 Spec 1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HerdrEvent {
    /// A pane's agent status changed (idle/working/blocked/unknown).
    PaneAgentStatusChanged { pane_id: String, status: String },
    /// A pane exited: process gone.
    PaneExited { pane_id: String, herdr_name: String },
}

/// Minimal typed watch hit (the `toron watch --once` doorbell).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchHit {
    pub project: String,
    pub kind: String,
    #[serde(default)]
    pub event_id: Option<String>,
}

/// The one ingress type: Jido's sensor shape onto the spine we already run
/// (ADR-0025 Spec 6). Unification is the enum — no new bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IngressEvent {
    Herdr(HerdrEvent),
    Watch(WatchHit),
    WorkflowTrigger(Trigger),
    CronTick { name: String },
}

/// Adapter sinks [`handle_ingress`] fans out to. Injected, never global.
/// The one ingress type: Jido's sensor shape onto the spine we already run
/// (ADR-0025 Spec 6). Unification is the enum — no new bus. Adjacent
/// tagging so inner enums (HerdrEvent's own `type` tag) never collide.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "ingress", content = "payload", rename_all = "snake_case")]
pub enum IngressEvent {
    pub workflow: Box<dyn Fn(&Trigger) + Send + Sync>,
    pub cron: Box<dyn Fn(&str) + Send + Sync>,
}

/// One router: every external fact enters the system here. A `pane_exited`
/// and a `message_posted` trigger cannot bypass it — they ARE this enum.
pub fn handle_ingress(event: &IngressEvent, adapters: &IngressAdapters) {
    match event {
        IngressEvent::Herdr(e) => (adapters.herdr)(e),
        IngressEvent::Watch(w) => (adapters.watch)(w),
        IngressEvent::WorkflowTrigger(t) => (adapters.workflow)(t),
        IngressEvent::CronTick { name } => (adapters.cron)(name),
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
        ];
        for e in events {
            let json = serde_json::to_string(&e).expect("serialize");
            let back: IngressEvent = serde_json::from_str(&json).expect("deserialize");
            let back_json = serde_json::to_string(&back).expect("serialize back");
            assert_eq!(back_json, json, "round trip lossless for {json}");
        }
    }
}
