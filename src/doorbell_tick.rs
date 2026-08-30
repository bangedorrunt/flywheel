// governed-by: ADR-0025
//! Doorbell Elm tick (GS-007 yw4s, GS-006 §10) — pure `tick(Model,Msg)->(Model,Actions)`

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum DoorbellMsg {
    WatchNdjson { line: String },
    InboxItems { raw: String, pin: String },
    HerdrStatus { pin: String, status: String },
    Fact { summary: String },
    AckSlaTick { now: u64 },
    WatchDied,
    BudgetReset { now: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum DoorbellAction {
    Prompt {
        pin: String,
        line: String,
    },
    FetchInbox {
        since: String,
    },
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

#[derive(Debug, Clone, Default)]
pub struct DoorbellModel {
    pub queues: BTreeMap<String, Vec<String>>,
    pub statuses: BTreeMap<String, String>,
}

pub fn tick(model: &DoorbellModel, msg: &DoorbellMsg) -> (DoorbellModel, Vec<DoorbellAction>) {
    let mut next = model.clone();
    let mut actions = Vec::new();
    match msg {
        DoorbellMsg::WatchNdjson { line } => {
            let _ = line;
        }
        DoorbellMsg::InboxItems { raw: _, pin } => {
            let _ = pin;
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
            actions.push(DoorbellAction::FactMiss {
                why: format!("fact without alias: {}", summary),
            });
        }
        DoorbellMsg::AckSlaTick { .. } => {
            actions.push(DoorbellAction::FetchInbox {
                since: "ack-sla".into(),
            });
        }
        DoorbellMsg::WatchDied => {
            actions.push(DoorbellAction::RestartWatch);
        }
        DoorbellMsg::BudgetReset { .. } => {}
    }
    (next, actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn doorbell_tick_is_pure_and_deterministic() {
        let model = DoorbellModel::default();
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
}
