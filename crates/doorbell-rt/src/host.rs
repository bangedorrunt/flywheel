// governed-by: ADR-0012
//! Host seams — everything flywheel-shaped the brain needs, behind traits.
//! bd-asr2.5 binds the real `workspace::tick`, spawn registry, herdr prompt,
//! and toron mailbox here without touching brain logic.

use crate::msg::BrainMsg;
use std::collections::BTreeMap;

/// Effect intents the brain journals, then executes through the [`Host`].
/// Mirrors `doorbell-core::DoorbellAction` plus the live loop's extra arms.
#[derive(Debug, Clone)]
pub enum Effect {
    /// The only delivery intent; sinks translate it (P1 corollary).
    Prompt { pin: String, line: String },
    /// Fetch plane-P inbox off-loop; rows re-enter as `InboxFetched`.
    /// Signal only (bd-z5ou): per-pin cursors live in the brain.
    FetchInbox,
    /// Run the ack-SLA scan; results re-enter as `SlaFired`.
    CheckAckSla,
    /// Restart the watch child with backoff.
    RestartWatch,
    /// Loud miss: journal + stderr, never silent `Ok(())`. Pin is
    /// first-class (bd-z5ou, same shape as the workspace `Cmd::Audit`);
    /// pin-less misses (fact without captain, registry ambiguity) use "".
    Audit {
        kind: String,
        pin: String,
        detail: serde_json::Value,
    },
    /// bd-bk9t: a wake for a pin with no prompt route (subscribed, never
    /// spawned, so no pane). Not a Prompt — the sink cannot deliver it — and
    /// not an Audit, because the wake is real and must reach the human. The
    /// brain folds it into the digest batch; `why` names the skip in the
    /// journal row.
    Digest {
        pin: String,
        line: String,
        why: String,
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
    /// Restart the wake consumer with backoff (GS-007 never-deaf rule).
    /// The brain calls this on `Died`; results re-enter as watch Msgs.
    ///
    /// Returns `true` when this call actually took the restart slot, i.e. a
    /// restart was performed; `false` when the death coalesced into one
    /// already pending (every real host is single-flight). The brain journals
    /// a `RestartWatch` row only for `true`, so the row and the `/health`
    /// count mean restarts, not deaths (bd-watch-restart-metric-8dvf).
    fn restart_wake(&mut self) -> bool;
    /// Binary-generation gate (.5: `WatcherGen`; default allows).
    fn inject_allowed(&self, _pin: &str) -> bool {
        true
    }
    /// Does `pin` have a prompt route — a pane/target the sink can reach?
    /// bd-bk9t: a digest batch for a pin with no route has nowhere to be
    /// shipped. The herdr sink MISSES an unknown pin and still returns
    /// `Ok`, so flushing would stamp a `Delivered` receipt (and a hear
    /// stamp) for a wake nobody received. The brain keeps those batches
    /// journal-recorded only — read them with `flywheel doorbell digest`.
    /// Default true: only a host with a routing table can answer.
    fn has_route(&self, _pin: &str) -> bool {
        true
    }
    /// The sink a wake for `pin` must go to, named as a registered adapter.
    ///
    /// bd-8ulu: the subscription row's `sink` is the operator's choice
    /// (identity x scope x sink), so it outranks the workspace-wide default.
    /// `None` means "no row names one" and the brain falls back to
    /// [`crate::brain::BrainConfig::main_sink`] — the default keeps every
    /// pre-existing single-sink deployment working unchanged.
    fn sink_for(&self, _pin: &str) -> Option<String> {
        None
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
    /// Pins the brain owns (fetch_all_inbox iterates these).
    pub pin_list: Vec<String>,
    /// Scripted tick replies, popped per call; exhausted → empty.
    pub script: std::collections::VecDeque<Vec<Effect>>,
    pub tick_log: Vec<String>,
    pub restarts: u32,
    /// Single-flight gate, mirroring `FileHost`: while a restart is pending,
    /// further deaths coalesce instead of scheduling their own. Tests clear it
    /// to model the gate reopening.
    pub restart_pending: bool,
    /// Last-known status per pin, recorded at tick time (mirrors the
    /// real hosts so `prev_status` ordering is testable at the brain).
    pub statuses: BTreeMap<String, String>,
    /// Pins with no prompt route (`has_route` false) — bd-bk9t tests.
    pub unroutable: std::collections::BTreeSet<String>,
}

impl MockHost {
    pub fn new(now: u64) -> Self {
        Self {
            now,
            inbox: Default::default(),
            sla: Vec::new(),
            pin_list: Vec::new(),
            script: Default::default(),
            tick_log: Vec::new(),
            restarts: 0,
            restart_pending: false,
            statuses: Default::default(),
            unroutable: Default::default(),
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
        // Record the new status at tick time (mirrors FileHost/RtHost):
        // a pre-tick `prev_status` read sees the previous value.
        if let BrainMsg::HerdrStatus { pin, status } = msg {
            self.statuses.insert(pin.clone(), status.clone());
        }
        self.script.pop_front().unwrap_or_default()
    }

    fn prev_status(&self, pin: &str) -> Option<String> {
        self.statuses.get(pin).cloned()
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

    fn pins(&self) -> Vec<String> {
        self.pin_list.clone()
    }

    fn scan_ack_sla(&mut self) -> Vec<String> {
        std::mem::take(&mut self.sla)
    }

    fn restart_wake(&mut self) -> bool {
        if self.restart_pending {
            return false;
        }
        self.restart_pending = true;
        self.restarts += 1;
        true
    }

    fn has_route(&self, pin: &str) -> bool {
        !self.unroutable.contains(pin)
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
            | BrainMsg::Delivered { .. }
            | BrainMsg::Ask { .. }
            | BrainMsg::Shutdown => return vec![],
        };
        let (next, actions) = doorbell_core::tick(&self.model, &core_msg);
        self.model = next;
        actions
            .into_iter()
            .map(|a| match a {
                DoorbellAction::Prompt { pin, line } => Effect::Prompt { pin, line },
                DoorbellAction::FetchInbox => Effect::FetchInbox,
                DoorbellAction::CheckAckSla => Effect::CheckAckSla,
                DoorbellAction::RestartWatch => Effect::RestartWatch,
                DoorbellAction::FactMiss { why } => Effect::Audit {
                    kind: "fact_miss".into(),
                    pin: String::new(),
                    detail: serde_json::json!({ "why": why }),
                },
                DoorbellAction::Audit { kind, pin, detail } => Effect::Audit {
                    kind,
                    pin,
                    detail: serde_json::json!(detail),
                },
                DoorbellAction::Digest { pin, line, why } => Effect::Digest { pin, line, why },
            })
            .collect()
    }
}
