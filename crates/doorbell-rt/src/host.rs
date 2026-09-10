// governed-by: ADR-0012
//! Host seams — everything flywheel-shaped the brain needs, behind traits.
//! bd-asr2.5 binds the real `workspace::tick`, spawn registry, herdr prompt,
//! and toron mailbox here without touching brain logic.

use crate::msg::BrainMsg;

/// Effect intents the brain journals, then executes through the [`Host`].
/// Mirrors `doorbell-core::DoorbellAction` plus the live loop's extra arms.
#[derive(Debug, Clone)]
pub enum Effect {
    /// The only delivery intent; sinks translate it (P1 corollary).
    Prompt { pin: String, line: String },
    /// Fetch plane-P inbox off-loop; rows re-enter as `InboxFetched`.
    FetchInbox { pin: String, since: String },
    /// Run the ack-SLA scan; results re-enter as `SlaFired`.
    CheckAckSla,
    /// Restart the watch child with backoff.
    RestartWatch,
    /// Loud miss: journal + stderr, never silent `Ok(())`.
    Audit {
        kind: String,
        detail: serde_json::Value,
    },
}

/// Flywheel-shaped capabilities the brain orchestrates. All synchronous:
/// sensors do I/O off-loop and deliver results back as messages (pipe).
pub trait Host: Send {
    /// Pure decision: message in, effect intents out (.5: `workspace::tick`).
    fn tick(&mut self, msg: &BrainMsg) -> Vec<Effect>;
    /// Wall-clock unix secs (enters tick only as payload, replayable).
    fn now_unix(&self) -> u64;
    /// Plane-P inbox rows for one pin (F6: shared borrow — fetch needs no
    /// &mut; pipe worker in .5 calls this off-loop, immediate here).
    fn fetch_inbox(&self, pin: &str, since: &str) -> Vec<serde_json::Value>;
    fn scan_ack_sla(&mut self) -> Vec<String>;
    /// Restart the watch child with backoff (GS-007 never-deaf rule).
    /// The brain calls this on `Died`; results re-enter as watch Msgs.
    fn restart_watch(&mut self);
    /// Binary-generation gate (.5: `WatcherGen`; default allows).
    fn inject_allowed(&self, _pin: &str) -> bool {
        true
    }
    /// Pins this brain owns (.5: spawn registry + role binds).
    fn pins(&self) -> Vec<String> {
        Vec::new()
    }
    /// Npub for hear tracking (.5: SelfIndex; default unknown).
    fn pin_npub(&self, _pin: &str) -> Option<String> {
        None
    }
    /// Last-known status for the transition gate (.5: Model statuses).
    fn prev_status(&self, _pin: &str) -> Option<String> {
        None
    }
    /// Per-sweep maintenance: registry/index refresh, forwarder respawn.
    /// Returns presence rows (pin, state) for trusted-edge ticks.
    fn sweep_maintenance(&mut self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Live role-bind/fact drains at the top of each sweep.
    fn drain_role_bounds(&mut self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn drain_facts(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// In-memory host for tests and the determinism proof.
pub struct MockHost {
    pub now: u64,
    pub inbox: std::collections::HashMap<(String, String), Vec<serde_json::Value>>,
    pub sla: Vec<String>,
    /// Scripted tick replies, popped per call; exhausted → empty.
    pub script: std::collections::VecDeque<Vec<Effect>>,
    pub tick_log: Vec<String>,
    pub restarts: u32,
}

impl MockHost {
    pub fn new(now: u64) -> Self {
        Self {
            now,
            inbox: Default::default(),
            sla: Vec::new(),
            script: Default::default(),
            tick_log: Vec::new(),
            restarts: 0,
        }
    }

    /// Tick through the REAL `doorbell-core` reducer: the same-Msg-log →
    /// same-Effects proof the Phase-1 parity harness asserts.
    pub fn with_core(now: u64) -> (Self, CoreReducer) {
        (Self::new(now), CoreReducer::default())
    }
}

impl Host for MockHost {
    fn tick(&mut self, msg: &BrainMsg) -> Vec<Effect> {
        self.tick_log.push(format!("{msg:?}"));
        self.script.pop_front().unwrap_or_default()
    }

    fn now_unix(&self) -> u64 {
        self.now
    }

    fn fetch_inbox(&self, pin: &str, since: &str) -> Vec<serde_json::Value> {
        self.inbox
            .get(&(pin.to_string(), since.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn scan_ack_sla(&mut self) -> Vec<String> {
        std::mem::take(&mut self.sla)
    }

    fn restart_watch(&mut self) {
        self.restarts += 1;
    }
}

/// Reducer-backed tick for the determinism proof: [`BrainMsg`] maps onto
/// `doorbell-core` messages 1:1 for the arms the core owns.
#[derive(Default)]
pub struct CoreReducer {
    pub model: doorbell_core::DoorbellModel,
}

impl CoreReducer {
    pub fn tick_effects(&mut self, msg: &BrainMsg) -> Vec<Effect> {
        use doorbell_core::{DoorbellAction, DoorbellMsg};
        let core_msg = match msg {
            BrainMsg::Watch(line) => DoorbellMsg::WatchNdjson { line: line.clone() },
            BrainMsg::HerdrStatus { pin, status } => DoorbellMsg::HerdrStatus {
                pin: pin.clone(),
                status: status.clone(),
            },
            BrainMsg::Fact { summary } => DoorbellMsg::Fact {
                summary: summary.clone(),
            },
            BrainMsg::AckSlaTick { now } => DoorbellMsg::AckSlaTick { now: *now },
            BrainMsg::Died => DoorbellMsg::WatchDied,
            BrainMsg::Sweep { .. }
            | BrainMsg::RoleBound { .. }
            | BrainMsg::Presence { .. }
            | BrainMsg::InboxFetched { .. }
            | BrainMsg::SlaFired { .. }
            | BrainMsg::Degraded { .. }
            | BrainMsg::Ask { .. }
            | BrainMsg::Shutdown => return vec![],
        };
        let (next, actions) = doorbell_core::tick(&self.model, &core_msg);
        self.model = next;
        actions
            .into_iter()
            .filter_map(|a| match a {
                DoorbellAction::Prompt { pin, line } => Some(Effect::Prompt { pin, line }),
                DoorbellAction::FetchInbox { since } => Some(Effect::FetchInbox {
                    pin: String::new(),
                    since,
                }),
                DoorbellAction::CheckAckSla => Some(Effect::CheckAckSla),
                DoorbellAction::RestartWatch => Some(Effect::RestartWatch),
                DoorbellAction::FactMiss { why } => Some(Effect::Audit {
                    kind: "fact_miss".into(),
                    detail: serde_json::json!({ "why": why }),
                }),
                DoorbellAction::Audit { kind, pin, detail } => Some(Effect::Audit {
                    kind,
                    detail: serde_json::json!({ "pin": pin, "detail": detail }),
                }),
            })
            .collect()
    }
}
