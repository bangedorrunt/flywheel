// governed-by: ADR-0025
//! Doorbell Elm tick (GS-007 yw4s, GS-006 §10) — pure `tick(Model,Msg)->(Model,Actions)`
//! The live dispatcher process (`cmd_internal_doorbell`) is the effect layer;
//! this module is the functional core (no I/O, no &mut). Tests assert
//! `same Msg log -> byte-identical Action sequence`. Classification reuses the
//! already-pure `crate::doorbell::{classify, parse_watch_ndjson, parse_inbox_item}`
//! — no second reducer.

use std::collections::BTreeMap;

/// One doorbell message. Every bus/inbox/status edge is a Msg; nothing
/// happens outside `tick`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum DoorbellMsg {
    /// Plane-S watch NDJSON line (`{kind,id,tags,content}`).
    WatchNdjson { line: String },
    /// Plane-P inbox rows for one pin (raw `fetch_inbox` JSON).
    InboxItems { raw: String, pin: String },
    /// Plane-P sweep path: rows already parsed (no serde round-trip).
    InboxRows {
        rows: Vec<serde_json::Value>,
        pin: String,
    },
    /// Herdr status change for one pin.
    HerdrStatus { pin: String, status: String },
    /// Captain-bound fact (no alias info here — honest FactMiss).
    Fact { summary: String },
    /// Periodic tick (60s cadence in the live dispatcher); drives the
    /// Ack-SLA scan effect — the scan itself is `check_ack_sla_expired`,
    /// NOT an inbox fetch (GS-009: naming lie fixed).
    AckSlaTick { now: u64 },
    /// The watch child died; supervisor must restart (never exit).
    WatchDied,
}

/// What the effect layer should do. Prompt is the only delivery; FetchInbox /
/// CheckAckSla / RestartWatch are effects whose results re-enter as Msgs;
/// FactMiss and Audit are loud misses (journal + stderr), never silent Ok(()).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum DoorbellAction {
    Prompt {
        pin: String,
        line: String,
    },
    FetchInbox {
        since: String,
    },
    /// Effect: run `check_ack_sla_expired` for the project (toron mailbox).
    /// Overdue wakes re-enter as WatchNdjson Msgs via plane S.
    CheckAckSla,
    RestartWatch,
    FactMiss {
        why: String,
    },
    Audit {
        kind: String,
        pin: String,
        detail: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DoorbellModel {
    /// LWW queues per pin (working panes hold their wake here).
    pub queues: BTreeMap<String, Vec<String>>,
    /// Last-known herdr status per pin.
    pub statuses: BTreeMap<String, String>,
    /// Pins (+npubs) this dispatcher owns.
    pub index: crate::doorbell::SelfIndex,
}

/// Expand an inbox JSON blob into row values: array, single object, or the
/// `{"messages":[...]}` wrapper `fetch_inbox` emits.
fn expand_inbox_rows(v: &serde_json::Value) -> Vec<&serde_json::Value> {
    match v {
        serde_json::Value::Array(arr) => arr.iter().collect(),
        other if other.get("messages").is_some() => other
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|arr| arr.iter().collect())
            .unwrap_or_default(),
        other => vec![other],
    }
}

/// Pure, total, deterministic: no I/O, no &mut, no herdr/toron calls.
pub fn tick(model: &DoorbellModel, msg: &DoorbellMsg) -> (DoorbellModel, Vec<DoorbellAction>) {
    let mut next = model.clone();
    let mut actions = Vec::new();
    match msg {
        DoorbellMsg::WatchNdjson { line } => {
            let Some((class, hint)) = crate::doorbell::parse_watch_ndjson(line, &next.index) else {
                return (next, actions);
            };
            let line_out = crate::doorbell::doorbell_line(class, &hint);
            for pin in crate::doorbell::watch_targets(line, &next.index) {
                route_or_queue(&mut next, &mut actions, &pin, line_out.clone());
            }
        }
        DoorbellMsg::InboxItems { raw, pin } => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
                actions.push(DoorbellAction::Audit {
                    kind: "inbox_parse_failure".into(),
                    pin: pin.clone(),
                    detail: raw.clone(),
                });
                return (next, actions);
            };
            for item in expand_inbox_rows(&v) {
                let Some((class, hint)) = crate::doorbell::parse_inbox_item(item) else {
                    continue;
                };
                let line_out = crate::doorbell::doorbell_line(class, &hint);
                route_or_queue(&mut next, &mut actions, pin, line_out);
            }
        }
        DoorbellMsg::InboxRows { rows, pin } => {
            // Plane-P sweep path: rows already parsed (Values), no round-trip.
            for item in rows {
                let Some((class, hint)) = crate::doorbell::parse_inbox_item(item) else {
                    continue;
                };
                let line_out = crate::doorbell::doorbell_line(class, &hint);
                route_or_queue(&mut next, &mut actions, pin, line_out);
            }
        }
        DoorbellMsg::HerdrStatus { pin, status } => {
            let from = next
                .statuses
                .get(pin)
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            if crate::doorbell::is_drain_transition(&from, status) {
                if let Some(q) = next.queues.get_mut(pin) {
                    for line in std::mem::take(q) {
                        actions.push(DoorbellAction::Prompt {
                            pin: pin.clone(),
                            line,
                        });
                    }
                }
            }
            next.statuses.insert(pin.clone(), status.clone());
        }
        DoorbellMsg::Fact { summary } => {
            // The doorbell-side reducer has no captain alias — an alias-less
            // fact is a LOUD miss (never registry.first()).
            actions.push(DoorbellAction::FactMiss {
                why: format!("fact without captain alias: {}", summary),
            });
        }
        DoorbellMsg::AckSlaTick { .. } => {
            // The scan is check_ack_sla_expired (toron), not an inbox fetch.
            // Overdue wakes arrive back as kind-9 ack-sla-expired WatchNdjson.
            actions.push(DoorbellAction::CheckAckSla);
        }
        DoorbellMsg::WatchDied => {
            // Never exit the dispatcher: restart is the only path (heal_tick is backup).
            actions.push(DoorbellAction::RestartWatch);
        }
    }
    (next, actions)
}

/// Idle/done/blocked panes get a Prompt now; working panes queue (drained on
/// the next working -> idle|done|blocked HerdrStatus).
fn route_or_queue(
    next: &mut DoorbellModel,
    actions: &mut Vec<DoorbellAction>,
    pin: &str,
    line: String,
) {
    let status = next
        .statuses
        .get(pin)
        .map(String::as_str)
        .unwrap_or("unknown");
    if crate::doorbell::should_inject(status) {
        actions.push(DoorbellAction::Prompt {
            pin: pin.to_string(),
            line,
        });
    } else {
        next.queues.entry(pin.to_string()).or_default().push(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with(pin: &str) -> DoorbellModel {
        let mut m = DoorbellModel::default();
        m.index.insert_pin(pin.to_string(), None);
        m.statuses.insert(pin.to_string(), "idle".into());
        m
    }

    #[test]
    fn doorbell_tick_is_pure_and_deterministic() {
        let model = model_with("BlueRidge");
        let msg = DoorbellMsg::WatchDied;
        let (m1, a1) = tick(&model, &msg);
        let (m2, a2) = tick(&model, &msg);
        assert_eq!(a1, a2);
        assert_eq!(m1.queues.len(), m2.queues.len());
    }

    #[test]
    fn doorbell_tick_watch_died_produces_restart() {
        let model = DoorbellModel::default();
        let (_, actions) = tick(&model, &DoorbellMsg::WatchDied);
        assert!(actions.contains(&DoorbellAction::RestartWatch));
    }

    #[test]
    fn doorbell_tick_watch_line_prompts_idle_pin() {
        let model = model_with("BlueRidge");
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            DoorbellAction::Prompt { pin, line } => {
                assert_eq!(pin, "BlueRidge");
                assert!(line.contains("[doorbell]"));
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
        assert!(next.queues.is_empty(), "idle pin must not queue");
    }

    #[test]
    fn doorbell_tick_watch_line_queues_working_pin_then_drains() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "working".into());
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert!(actions.is_empty(), "working pin must not prompt");
        assert_eq!(next.queues.get("BlueRidge").map(Vec::len), Some(1));
        let (next2, actions2) = tick(
            &next,
            &DoorbellMsg::HerdrStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert_eq!(actions2.len(), 1);
        assert!(actions2[0].to_label().contains("BlueRidge"));
        assert!(next2.queues.get("BlueRidge").is_none_or(|q| q.is_empty()));
    }

    #[test]
    fn doorbell_tick_mail_delivered_pulls_inbox_not_channel() {
        let model = model_with("BlueRidge");
        let line = r#"{"kind":9,"id":"aa","tags":[["p","BlueRidge"],["toron-kind","mail-delivered"],["h","t-1"]],"content":"sealed"}"#;
        let (_, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            DoorbellAction::Prompt { line, .. } => {
                assert!(line.contains("inbox"), "mail wake must pull inbox: {line}");
                assert!(
                    !line.contains("channel read"),
                    "mail wake must not channel read: {line}"
                );
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn doorbell_tick_room_chatter_is_silent() {
        let mut model = model_with("BlueRidge");
        model.index.insert_pin("LilacChapel".into(), None);
        let line = r#"{"kind":9,"id":"cc","tags":[["h","ch-1"]],"content":"room talk"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert!(actions.is_empty(), "unaddressed chatter must not ring");
        assert!(next.queues.is_empty());
    }

    #[test]
    fn doorbell_tick_inbox_messages_wrapper_parses() {
        // fetch_inbox also emits {"messages":[...]} — must expand, not treat
        // the wrapper as one unparseable row.
        let model = model_with("BlueRidge");
        let raw = r#"{"messages":[{"importance":"urgent"},{"importance":"high"},{"importance":"normal"}]}"#;
        let (_, actions) = tick(
            &model,
            &DoorbellMsg::InboxItems {
                raw: raw.into(),
                pin: "BlueRidge".into(),
            },
        );
        assert_eq!(actions.len(), 2, "urgent+high ring, normal stays quiet");
    }

    #[test]
    fn doorbell_tick_inbox_rows_prompts_idle_pin() {
        let model = model_with("BlueRidge");
        let rows = vec![
            serde_json::json!({"importance":"urgent"}),
            serde_json::json!({"importance":"high"}),
            serde_json::json!({"importance":"normal"}),
        ];
        let (_, actions) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows,
                pin: "BlueRidge".into(),
            },
        );
        assert_eq!(actions.len(), 2, "urgent+high ring, normal stays quiet");
        for a in &actions {
            assert!(a.to_label().contains("BlueRidge"));
        }
    }

    #[test]
    fn doorbell_tick_bad_inbox_json_is_loud_audit() {
        let model = model_with("BlueRidge");
        let (_, actions) = tick(
            &model,
            &DoorbellMsg::InboxItems {
                raw: "{not json".into(),
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "inbox_parse_failure"),
            "parse failure must be a loud audit, got {:?}",
            actions
        );
    }
}

impl DoorbellAction {
    /// Test/debug label (also keeps the enum honest in logs).
    pub fn to_label(&self) -> String {
        match self {
            DoorbellAction::Prompt { pin, .. } => format!("prompt {pin}"),
            DoorbellAction::FetchInbox { since } => format!("fetch-inbox {since}"),
            DoorbellAction::CheckAckSla => "check-ack-sla".into(),
            DoorbellAction::RestartWatch => "restart-watch".into(),
            DoorbellAction::FactMiss { why } => format!("fact-miss {why}"),
            DoorbellAction::Audit { kind, pin, .. } => format!("audit {kind} {pin}"),
        }
    }
}
