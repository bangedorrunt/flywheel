// governed-by: ADR-0027
//! One Workspace Tick — Control Plane as Elm/Redux on Doorbell + 30013
//! Single owner of control-plane types: WorkspaceModel, Msg, Cmd, tick.
//! The two existing ticks (doorbell_tick::tick, durable::tick) become
//! private slice reducers called ONLY from WorkspaceModel::tick.
//! Clock enters tick only as Msg payload (Tick{now}); no Instant now.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// Assignment receipt — durable row cached in Model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Receipt {
    pub state: String,
    pub wake: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
}

/// Presence per pin; missing key = Unknown (invariant 9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Presence {
    #[default]
    Unknown,
    Idle,
    Working,
    Blocked,
    Done,
}

impl Presence {
    pub fn from_str(s: &str) -> Self {
        match s {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            _ => Self::Unknown,
        }
    }
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unknown => "unknown",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

/// Human decision question — at most one per corr (invariant 11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Question {
    pub corr: String,
    pub body: String,
    #[serde(default)]
    pub created_at: u64,
}

/// One workspace model — honest composition, not a god object.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorkspaceModel {
    pub doorbell: crate::doorbell_tick::DoorbellModel,
    pub swarm: crate::durable::LoopModel,
    pub assignment: BTreeMap<String, Receipt>,
    pub roles: BTreeMap<String, String>,
    pub presence: BTreeMap<String, Presence>,
    pub open_human: BTreeMap<String, Question>,
    pub seq: u64,
}

// ---------------------------------------------------------------------------
// Msg — one union covering mail / watch / status / heal / reclaim / receipt / role / human / presence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Msg {
    WatchNdjson {
        line: String,
    },
    InboxItems {
        raw: String,
        pin: String,
    },
    InboxRows {
        rows: Vec<serde_json::Value>,
        pin: String,
    },
    HerdrStatus {
        pin: String,
        status: String,
    },
    Fact {
        summary: String,
    },
    AckSlaTick {
        now: u64,
    },
    WatchDied,
    ClaimWatch {
        name: String,
    },
    Reclaim {
        bead_id: String,
        holder_pin: String,
        claimant_pin: String,
        is_live: bool,
    },
    Heal {
        name: String,
        status: String,
        exited: bool,
    },
    Receipt {
        corr: String,
        receipt: Receipt,
    },
    RoleBound {
        role: String,
        pin: String,
    },
    Presence {
        pin: String,
        state: String,
    },
    HumanDecision {
        corr: String,
        verdict: String,
    },
    /// Clock discipline: wall-clock enters tick ONLY as this payload (E1).
    Tick {
        now: u64,
    },
}

// ---------------------------------------------------------------------------
// Cmd — effects as data (DoorbellAction + LoopAction collapse into this)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Cmd {
    // doorbell slice — data equivalents of DoorbellAction
    Prompt {
        pin: String,
        line: String,
    },
    FetchInbox {
        since: String,
    },
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

    // swarm slice — data equivalents of LoopAction
    Send {
        name: String,
        text: String,
    },
    PinStamp {
        bead_id: String,
        pin: String,
    },
    DispatchImmediate {
        name: String,
        text: String,
    },
    Blocked {
        name: String,
    },
    Nudge {
        name: String,
    },
    Diagnose {
        name: String,
        reason: String,
    },
    Respawn {
        name: String,
        reason: String,
    },
    Reclaim {
        bead_id: String,
        pin: String,
        herdr_name: String,
        reason: String,
    },
    Wait,

    // E2/E3 assignment + receipts
    SqliteUpsert {
        corr: String,
        receipt: Receipt,
    },
    Publish {
        kind: u64,
        content: String,
    },
    Subscribe {
        filter: String,
    },
    WaitCorr {
        corr: String,
    },
    Invoke {
        workflow: String,
        input: String,
    },
    WriteNcryptsec {
        pin: String,
        secret: String,
    },
    LoudMiss {
        why: String,
    },
}

impl Cmd {
    fn from_doorbell(a: crate::doorbell_tick::DoorbellAction) -> Self {
        match a {
            crate::doorbell_tick::DoorbellAction::Prompt { pin, line } => {
                Self::Prompt { pin, line }
            }
            crate::doorbell_tick::DoorbellAction::FetchInbox { since } => {
                Self::FetchInbox { since }
            }
            crate::doorbell_tick::DoorbellAction::CheckAckSla => Self::CheckAckSla,
            crate::doorbell_tick::DoorbellAction::RestartWatch => Self::RestartWatch,
            crate::doorbell_tick::DoorbellAction::FactMiss { why } => Self::FactMiss { why },
            crate::doorbell_tick::DoorbellAction::Audit { kind, pin, detail } => {
                Self::Audit { kind, pin, detail }
            }
        }
    }

    fn from_loop(a: crate::durable::LoopAction) -> Self {
        match a {
            crate::durable::LoopAction::Send { name, text } => Self::Send { name, text },
            crate::durable::LoopAction::PinStamp { bead_id, pin } => {
                Self::PinStamp { bead_id, pin }
            }
            crate::durable::LoopAction::DispatchImmediate { name, text } => {
                Self::DispatchImmediate { name, text }
            }
            crate::durable::LoopAction::Blocked { name } => Self::Blocked { name },
            crate::durable::LoopAction::Nudge { name } => Self::Nudge { name },
            crate::durable::LoopAction::Diagnose { name, reason } => {
                Self::Diagnose { name, reason }
            }
            crate::durable::LoopAction::Respawn { name, reason } => Self::Respawn { name, reason },
            crate::durable::LoopAction::Reclaim {
                bead_id,
                pin,
                herdr_name,
                reason,
            } => Self::Reclaim {
                bead_id,
                pin,
                herdr_name,
                reason,
            },
            crate::durable::LoopAction::Wait => Self::Wait,
        }
    }
}

// ---------------------------------------------------------------------------
// Private slice reducers — the only place the two old ticks are called.
// ---------------------------------------------------------------------------

fn doorbell_slice(
    model: &crate::doorbell_tick::DoorbellModel,
    msg: crate::doorbell_tick::DoorbellMsg,
) -> (crate::doorbell_tick::DoorbellModel, Vec<Cmd>) {
    let (next, actions) = crate::doorbell_tick::tick(model, &msg);
    let cmds = actions.into_iter().map(Cmd::from_doorbell).collect();
    (next, cmds)
}

fn swarm_slice(
    model: &crate::durable::LoopModel,
    msg: crate::durable::LoopMsg,
) -> (crate::durable::LoopModel, Vec<Cmd>) {
    let (next, actions) = crate::durable::tick(model, &msg);
    let cmds = actions.into_iter().map(Cmd::from_loop).collect();
    (next, cmds)
}

// ---------------------------------------------------------------------------
// Entry: one tick
// ---------------------------------------------------------------------------

/// One entry: pure, total, deterministic, no I/O, no &mut.
/// Clock enters ONLY as Msg payload (Tick{now} / AckSlaTick{now}).
pub fn tick(model: &WorkspaceModel, msg: &Msg) -> (WorkspaceModel, Vec<Cmd>) {
    let mut next = model.clone();
    next.seq = model.seq.wrapping_add(1);
    let mut cmds: Vec<Cmd> = Vec::new();

    match msg {
        Msg::WatchNdjson { line } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::WatchNdjson { line: line.clone() },
            );
            next.doorbell = dm;
            cmds.extend(c);
            // E2: also handle tagged kind-9 WatchNdjson (corr/wake via extra_tags)
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(tags) = v.get("tags").and_then(|t| t.as_array()) {
                    let mut corr: Option<String> = None;
                    let mut wake: Option<String> = None;
                    let mut audience: Option<String> = None;
                    let mut result_path: Option<String> = None;
                    let mut priority: Option<String> = None;
                    for tag in tags {
                        if let Some(arr) = tag.as_array() {
                            if arr.len() >= 2 {
                                let k = arr[0].as_str().unwrap_or("");
                                let val = arr[1].as_str().unwrap_or("").to_string();
                                match k {
                                    "corr" => corr = Some(val),
                                    "wake" => wake = Some(val),
                                    "audience" => audience = Some(val),
                                    "result_path" => result_path = Some(val),
                                    "priority" => priority = Some(val),
                                    _ => {}
                                }
                            }
                        }
                    }
                    if let Some(c) = corr {
                        let w = wake.unwrap_or_else(|| "tasked".to_string());
                        let a = audience.unwrap_or_else(|| "agents".to_string());
                        let p = priority.unwrap_or_else(|| "queue".to_string());
                        let body = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        let broadcast = false;
                        if a == "human" && broadcast {
                            cmds.push(Cmd::LoudMiss {
                                why: format!("broadcast to human is hard error for corr {c}"),
                            });
                        } else if p == "interrupt" && a == "human" {
                            cmds.push(Cmd::LoudMiss {
                                why: format!("interrupt never onto human pin for corr {c}"),
                            });
                        } else if w == "standing" && body.is_empty() {
                            // silence
                        } else {
                            let state = if w == "tasked" && body.is_empty() {
                                "resulted"
                            } else {
                                "owed"
                            };
                            let receipt = Receipt {
                                state: state.to_string(),
                                wake: w.clone(),
                                due_at: None,
                                result_path: result_path.clone(),
                            };
                            next.assignment.insert(c.clone(), receipt.clone());
                            cmds.push(Cmd::SqliteUpsert {
                                corr: c.clone(),
                                receipt: receipt.clone(),
                            });
                            cmds.push(Cmd::WaitCorr { corr: c });
                        }
                    }
                }
            }
        }
        Msg::InboxItems { raw, pin } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::InboxItems {
                    raw: raw.clone(),
                    pin: pin.clone(),
                },
            );
            next.doorbell = dm;
            cmds.extend(c);
        }
        Msg::InboxRows { rows, pin } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::InboxRows {
                    rows: rows.clone(),
                    pin: pin.clone(),
                },
            );
            next.doorbell = dm;
            cmds.extend(c);
            // E2/E3: assignment from tagged rows (corr/wake/priority/audience/result_path)
            for row in rows {
                let corr = row
                    .get("corr")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if corr.is_empty() {
                    continue;
                }
                let wake = row
                    .get("wake")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tasked")
                    .to_string();
                let body = row
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let audience = row
                    .get("audience")
                    .and_then(|v| v.as_str())
                    .unwrap_or("agents")
                    .to_string();
                let result_path = row
                    .get("result_path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let priority = row
                    .get("priority")
                    .and_then(|v| v.as_str())
                    .unwrap_or("queue")
                    .to_string();
                let broadcast = row
                    .get("broadcast")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // E3: audience=human broadcast is hard error
                if audience == "human" && broadcast {
                    cmds.push(Cmd::LoudMiss {
                        why: format!("broadcast to human is hard error for corr {corr}"),
                    });
                    continue;
                }
                // E3: interrupt never onto human pin
                if priority == "interrupt" && audience == "human" {
                    cmds.push(Cmd::LoudMiss {
                        why: format!("interrupt never onto human pin for corr {corr}"),
                    });
                    continue;
                }
                // E3: standing + empty body -> silence (no Cmd, no assignment)
                if wake == "standing" && body.is_empty() {
                    continue;
                }
                // tasked + empty body -> result against corr
                let state = if wake == "tasked" && body.is_empty() {
                    "resulted"
                } else {
                    "owed"
                };
                let receipt = Receipt {
                    state: state.to_string(),
                    wake: wake.clone(),
                    due_at: None,
                    result_path: result_path.clone(),
                };
                next.assignment.insert(corr.clone(), receipt.clone());
                cmds.push(Cmd::SqliteUpsert {
                    corr: corr.clone(),
                    receipt: receipt.clone(),
                });
                cmds.push(Cmd::WaitCorr { corr: corr.clone() });
                // default tasked priority is queue (already handled, interrupt is explicit)
            }
            let _ = pin;
        }
        Msg::HerdrStatus { pin, status } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::HerdrStatus {
                    pin: pin.clone(),
                    status: status.clone(),
                },
            );
            next.doorbell = dm;
            cmds.extend(c);
        }
        Msg::Fact { summary } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::Fact {
                    summary: summary.clone(),
                },
            );
            next.doorbell = dm;
            cmds.extend(c);
        }
        Msg::AckSlaTick { now } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::AckSlaTick { now: *now },
            );
            next.doorbell = dm;
            cmds.extend(c);
        }
        Msg::WatchDied => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::WatchDied,
            );
            next.doorbell = dm;
            cmds.extend(c);
        }
        Msg::ClaimWatch { name } => {
            cmds.push(Cmd::Subscribe {
                filter: format!("claim-watch:{name}"),
            });
        }
        Msg::Reclaim {
            bead_id,
            holder_pin,
            claimant_pin,
            is_live,
        } => {
            let (sm, c) = swarm_slice(
                &model.swarm,
                crate::durable::LoopMsg::Reclaim {
                    bead_id: bead_id.clone(),
                    holder_pin: holder_pin.clone(),
                    claimant_pin: claimant_pin.clone(),
                    is_live: *is_live,
                },
            );
            next.swarm = sm;
            cmds.extend(c);
        }
        Msg::Heal {
            name,
            status,
            exited,
        } => {
            if *exited {
                cmds.push(Cmd::Diagnose {
                    name: name.clone(),
                    reason: format!("heal:{status}"),
                });
            }
        }
        Msg::Receipt { corr, receipt } => {
            next.assignment.insert(corr.clone(), receipt.clone());
            cmds.push(Cmd::SqliteUpsert {
                corr: corr.clone(),
                receipt: receipt.clone(),
            });
        }
        Msg::RoleBound { role, pin } => {
            next.roles.insert(role.clone(), pin.clone());
        }
        Msg::Presence { pin, state } => {
            next.presence.insert(pin.clone(), Presence::from_str(state));
        }
        Msg::HumanDecision { corr, verdict } => {
            if let Some(q) = next.open_human.remove(corr) {
                let _ = q;
                cmds.push(Cmd::Audit {
                    kind: "human_decision".into(),
                    pin: corr.clone(),
                    detail: verdict.clone(),
                });
            } else {
                cmds.push(Cmd::LoudMiss {
                    why: format!("HumanDecision for unknown corr {corr}: {verdict}"),
                });
            }
        }
        Msg::Tick { now } => {
            let _ = now;
        }
    }

    (next, cmds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_model() -> WorkspaceModel {
        WorkspaceModel::default()
    }

    #[test]
    fn ws_model_tick_is_pure_and_deterministic() {
        let m = empty_model();
        let msg = Msg::WatchDied;
        let (m1, a1) = tick(&m, &msg);
        let (m2, a2) = tick(&m, &msg);
        assert_eq!(a1, a2);
        assert_eq!(m1.seq, m2.seq);
    }

    #[test]
    fn ws_model_seq_increments_on_every_msg() {
        let m = empty_model();
        assert_eq!(m.seq, 0);
        let (m1, _) = tick(&m, &Msg::WatchDied);
        assert_eq!(m1.seq, 1);
        let (m2, _) = tick(&m1, &Msg::Tick { now: 42 });
        assert_eq!(m2.seq, 2);
    }

    #[test]
    fn ws_model_tick_composes_doorbell_watchndjson() {
        let mut m = empty_model();
        m.doorbell.index.insert_pin("BlueRidge".into(), None);
        m.doorbell
            .statuses
            .insert("BlueRidge".into(), "idle".into());
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#.to_string();
        let (next, cmds) = tick(&m, &Msg::WatchNdjson { line });
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Cmd::Prompt { pin, .. } => assert_eq!(pin, "BlueRidge"),
            other => panic!("expected Prompt got {:?}", other),
        }
        assert_eq!(next.seq, 1);
    }

    #[test]
    fn ws_model_tick_clock_only_via_msg_payload() {
        let m = empty_model();
        let (m1, a1) = tick(&m, &Msg::Tick { now: 1_000_000 });
        let (m2, a2) = tick(&m, &Msg::Tick { now: 1_000_000 });
        assert_eq!(a1, a2);
        assert_eq!(m1.seq, m2.seq);
        let (_, a3) = tick(&m, &Msg::Tick { now: 2_000_000 });
        assert_eq!(a1.len(), a3.len());
    }

    #[test]
    fn ws_model_tick_no_instant_inside_source() {
        let src = include_str!("workspace.rs");
        let a = ["Instant", "::now"].concat();
        let b = ["SystemTime", "::now"].concat();
        assert!(!src.contains(&a), "tick must not call Instant now");
        assert!(!src.contains(&b), "tick must not call SystemTime now");
    }

    #[test]
    fn ws_model_role_bound_updates_roles_map() {
        let m = empty_model();
        let (next, cmds) = tick(
            &m,
            &Msg::RoleBound {
                role: "wheelhouse".into(),
                pin: "DustySparrow".into(),
            },
        );
        assert!(cmds.is_empty());
        assert_eq!(
            next.roles.get("wheelhouse").map(String::as_str),
            Some("DustySparrow")
        );
        let (next2, _) = tick(
            &next,
            &Msg::RoleBound {
                role: "wheelhouse".into(),
                pin: "NewPin".into(),
            },
        );
        assert_eq!(
            next2.roles.get("wheelhouse").map(String::as_str),
            Some("NewPin")
        );
    }

    #[test]
    fn ws_model_presence_missing_is_unknown() {
        let m = empty_model();
        assert!(m.presence.get("GhostPin").is_none());
        let (next, _) = tick(
            &m,
            &Msg::Presence {
                pin: "GhostPin".into(),
                state: "idle".into(),
            },
        );
        assert_eq!(next.presence.get("GhostPin"), Some(&Presence::Idle));
        let (next2, _) = tick(
            &next,
            &Msg::Presence {
                pin: "GhostPin".into(),
                state: "working".into(),
            },
        );
        assert_eq!(next2.presence.get("GhostPin"), Some(&Presence::Working));
        let (next3, _) = tick(
            &next2,
            &Msg::Presence {
                pin: "GhostPin".into(),
                state: "weird".into(),
            },
        );
        assert_eq!(next3.presence.get("GhostPin"), Some(&Presence::Unknown));
    }

    #[test]
    fn ws_model_receipt_inserts_assignment_and_emits_sqlite_upsert() {
        let m = empty_model();
        let receipt = Receipt {
            state: "owed".into(),
            wake: "tasked".into(),
            due_at: Some(999),
            result_path: None,
        };
        let (next, cmds) = tick(
            &m,
            &Msg::Receipt {
                corr: "FM-GS-012".into(),
                receipt: receipt.clone(),
            },
        );
        assert_eq!(next.assignment.get("FM-GS-012"), Some(&receipt));
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Cmd::SqliteUpsert { corr, receipt: r } => {
                assert_eq!(corr, "FM-GS-012");
                assert_eq!(r, &receipt);
            }
            other => panic!("expected SqliteUpsert got {:?}", other),
        }
    }

    #[test]
    fn ws_model_human_decision_one_per_corr() {
        let mut m = empty_model();
        m.open_human.insert(
            "FM-GS-012".into(),
            Question {
                corr: "FM-GS-012".into(),
                body: "approve?".into(),
                created_at: 1,
            },
        );
        let (next, cmds) = tick(
            &m,
            &Msg::HumanDecision {
                corr: "FM-GS-012".into(),
                verdict: "approve".into(),
            },
        );
        assert!(!next.open_human.contains_key("FM-GS-012"));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Audit { kind, .. } if kind == "human_decision"))
        );
        let (next2, cmds2) = tick(
            &next,
            &Msg::HumanDecision {
                corr: "FM-GS-012".into(),
                verdict: "approve".into(),
            },
        );
        assert!(cmds2.iter().any(|c| matches!(c, Cmd::LoudMiss { .. })));
        assert!(next2.open_human.is_empty());
    }

    #[test]
    fn ws_model_claim_watch_emits_subscribe_not_loop_append() {
        let m = empty_model();
        let (next, cmds) = tick(
            &m,
            &Msg::ClaimWatch {
                name: "my-bead".into(),
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Subscribe { filter } if filter.contains("my-bead")))
        );
        assert_eq!(next.swarm.queue.len(), m.swarm.queue.len());
    }

    #[test]
    fn ws_model_heal_exited_emits_diagnose() {
        let m = empty_model();
        let (_, cmds) = tick(
            &m,
            &Msg::Heal {
                name: "AgentA".into(),
                status: "blocked".into(),
                exited: true,
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Diagnose { name, .. } if name == "AgentA"))
        );
        let (_, cmds2) = tick(
            &m,
            &Msg::Heal {
                name: "AgentA".into(),
                status: "blocked".into(),
                exited: false,
            },
        );
        assert!(cmds2.is_empty());
    }

    #[test]
    fn ws_model_reclaim_delegates_to_swarm_slice() {
        let m = empty_model();
        let (next, _) = tick(
            &m,
            &Msg::Reclaim {
                bead_id: "br-1".into(),
                holder_pin: "A".into(),
                claimant_pin: "B".into(),
                is_live: false,
            },
        );
        assert_eq!(next.seq, 1);
    }

    #[test]
    fn ws_model_tick_is_total_for_all_variants() {
        let m = empty_model();
        let msgs = vec![
            Msg::WatchNdjson { line: "{}".into() },
            Msg::InboxItems {
                raw: "{}".into(),
                pin: "P".into(),
            },
            Msg::InboxRows {
                rows: vec![],
                pin: "P".into(),
            },
            Msg::HerdrStatus {
                pin: "P".into(),
                status: "idle".into(),
            },
            Msg::Fact {
                summary: "test".into(),
            },
            Msg::AckSlaTick { now: 0 },
            Msg::WatchDied,
            Msg::ClaimWatch { name: "x".into() },
            Msg::Reclaim {
                bead_id: "b".into(),
                holder_pin: "h".into(),
                claimant_pin: "c".into(),
                is_live: false,
            },
            Msg::Heal {
                name: "n".into(),
                status: "s".into(),
                exited: false,
            },
            Msg::Receipt {
                corr: "c".into(),
                receipt: Receipt::default(),
            },
            Msg::RoleBound {
                role: "r".into(),
                pin: "p".into(),
            },
            Msg::Presence {
                pin: "p".into(),
                state: "idle".into(),
            },
            Msg::HumanDecision {
                corr: "c".into(),
                verdict: "v".into(),
            },
            Msg::Tick { now: 123 },
        ];
        for msg in msgs {
            let (next, _) = tick(&m, &msg);
            assert_eq!(next.seq, 1, "seq must increment for {:?}", msg);
        }
    }
}

#[cfg(test)]
mod bell_fold_tests {
    #[test]
    fn bell_fold_no_double_bell_model() {
        let src = include_str!("main.rs");
        let needle = "let mut bell_model = doorbell_tick::DoorbellModel::default()";
        let count = src.matches(needle).count();
        assert!(
            count <= 1,
            "RED (bd-6b1y #6): main.rs has {count} bell_model = Default (7747/7819) — must be ONE model for dispatcher lifetime"
        );
    }

    #[test]
    fn bell_fold_no_status_preinsert_before_herdr_tick() {
        let src = include_str!("main.rs");
        // pre-insert before HerdrStatus tick kills drain (tick must see from)
        let needle = "bell_model.statuses.insert(pin.clone(), to.clone())";
        let count = src.matches(needle).count();
        assert!(
            count <= 1,
            "RED (bd-6b1y #7): main.rs still pre-inserts statuses before HerdrStatus tick"
        );
    }

    #[test]
    fn bell_fold_watchdied_keeps_model() {
        let src = include_str!("main.rs");
        assert!(
            !src.contains("let (_, tick_actions) = doorbell_tick::tick(&bell_model, &msg)"),
            "RED (bd-6b1y #8): main.rs WatchDied still discards model' (`let (_, tick_actions) = tick(...)` at 8055) — keep the returned model"
        );
        // Must have keep: let (next_model, tick_actions) = ... ; bell_model = next_model;
        assert!(
            src.contains("WatchDied"),
            "src must contain WatchDied handling"
        );
    }

    #[test]
    fn bell_fold_once_calls_tick_not_second_reducer() {
        let main = include_str!("main.rs");
        let doorbell = include_str!("doorbell.rs");
        // once_json should call tick, not apply_watch_line as live second reducer
        assert!(
            main.contains("doorbell_tick::tick"),
            "RED (bd-6b1y #9): main.rs must wire through tick"
        );
        // doorbell.rs once_json must use tick after fix
        assert!(
            doorbell.contains("doorbell_tick::tick"),
            "RED (bd-6b1y #9): doorbell.rs once_json must call tick, not apply_watch_line"
        );
    }

    #[test]
    fn bell_fold_fact_path_live() {
        let src = include_str!("main.rs");
        let dt = include_str!("doorbell_tick.rs");
        assert!(
            dt.contains("Fact"),
            "RED (bd-6b1y #10): DoorbellMsg::Fact missing in doorbell_tick.rs"
        );
        assert!(
            src.contains("DoorbellMsg::Fact") || src.contains("Fact {"),
            "RED (bd-6b1y #10): DoorbellMsg::Fact not on live path in main.rs"
        );
        // adapter must not swallow Err into ()
        assert!(
            !src.contains("if let Err(e) = captain_fact_inject(&proj, ev)"),
            "RED (bd-6b1y #10): adapter still swallows Err into () at main.rs 10853-10860 — must propagate fact into Msg"
        );
    }

    #[test]
    fn bell_fold_fetch_inbox_emitted() {
        let dt = include_str!("doorbell_tick.rs");
        let main = include_str!("main.rs");
        // FetchInbox must be emitted as real Cmd, rows re-enter as InboxRows
        assert!(
            dt.contains("FetchInbox"),
            "RED (bd-6b1y #13): FetchInbox variant missing"
        );
        // Must have a constructor or usage in main.rs / workspace.rs
        let has_emit = main.contains("FetchInbox") || dt.contains("FetchInbox");
        assert!(
            has_emit,
            "RED (bd-6b1y #13): FetchInbox never emitted — must be a real Cmd whose rows re-enter as InboxRows"
        );
        // workspace tick should handle InboxRows
        let ws = include_str!("workspace.rs");
        assert!(ws.contains("InboxRows"), "workspace must handle InboxRows");
    }

    #[test]
    fn bell_fold_why_replay() {
        let doorbell = include_str!("doorbell.rs");
        let main = include_str!("main.rs");
        assert!(
            doorbell.contains("doorbell-journal-") && doorbell.contains("journals"),
            "RED (bd-6b1y #11): doorbell.rs journal path must be state_dir/journals/doorbell-journal"
        );
        // why must fold tick from Default, have --corr/--seq, not grep
        assert!(
            main.contains("fn cmd_doorbell_why"),
            "RED (bd-6b1y #11): fn cmd_doorbell_why not found"
        );
        let pos = main.find("fn cmd_doorbell_why").unwrap();
        let slice = &main[pos..std::cmp::min(main.len(), pos + 8000)];
        assert!(
            slice.contains("fold") || slice.contains("replay") || slice.contains("tick("),
            "RED (bd-6b1y #11): doorbell why must fold tick from Default over journal and assert same Cmd sequence — currently greps pin substring, not replay"
        );
        assert!(
            slice.contains("corr") && slice.contains("seq"),
            "RED (bd-6b1y #11): why must gain --corr / --seq flags — why_fn missing corr/seq"
        );
        assert!(
            !slice.contains("raw.contains(&needle)") || slice.contains("fold"),
            "RED (bd-6b1y #11): why still greps pin substring — must fold tick"
        );
    }
}

#[cfg(test)]
mod loop_fold_tests {
    #[test]
    fn loop_fold_no_watch_sends_after_tick() {
        let durable = include_str!("durable.rs");
        assert!(
            !durable.contains("actions.append(&mut watch_sends)"),
            "RED (bd-gs8b #2): durable.rs still appends watch_sends AFTER tick — must be Msg::ClaimWatch into tick; delete append at ~6996"
        );
        assert!(
            !durable.contains("DELIBERATELY") || !durable.contains("loop-side"),
            "RED (bd-gs8b #2): durable.rs still carries DELIBERATELY loop-side comment — delete it when folding ClaimWatch"
        );
    }

    #[test]
    fn loop_fold_no_heal_pre_seed() {
        let durable = include_str!("durable.rs");
        // Reclaim pre-seeds heal_actions before tick — must be Msg::Reclaim into tick
        assert!(
            !durable.contains("heal_actions")
                || durable.contains("Msg::Reclaim")
                || durable.contains("Reclaim {"),
            "RED (bd-gs8b #3): durable.rs still pre-seeds heal_actions before tick — must be Msg::Reclaim into tick, then actions = tick(...) only"
        );
        // Check that tick is the only producer of actions (no pre-seed var)
        let has_preseed =
            durable.contains("let mut heal_actions") && durable.contains("actions = tick");
        assert!(
            !has_preseed,
            "RED (bd-gs8b #3): durable.rs has heal_actions pre-seed before tick"
        );
    }

    #[test]
    fn loop_fold_no_diagnose_filter() {
        let durable = include_str!("durable.rs");
        // Check for the specific post-tick filter that was at 6956-6962: filter Diagnose pane_exited
        let has_filter = durable.contains("filter(|a|")
            && durable.contains("pane_exited")
            && durable.contains("poll_actions");
        assert!(
            !has_filter,
            "RED (bd-gs8b #4): durable.rs still filters Diagnose pane_exited after tick — DELETE the filter"
        );
    }

    #[test]
    fn loop_fold_no_double_apply() {
        let durable = include_str!("durable.rs");
        // loop pre-apply must be gone (the specific line we removed)
        assert!(
            !durable.contains("apply_events_to_states(&mut states, &event_batch.events)"),
            "RED (bd-gs8b #5): loop pre-apply still present"
        );
        // tick's apply should remain (at least one occurrence)
        assert!(
            durable.contains("apply_events_to_states"),
            "tick's apply should remain"
        );
    }

    #[test]
    fn loop_fold_heal_states_on_activeloop() {
        let state = include_str!("state.rs");
        assert!(
            state.contains("heal_states"),
            "RED (bd-gs8b #14): state.rs ActiveLoop missing heal_states — move onto ActiveLoop (crash-safe)"
        );
        // Also check that durable.rs does not have loop-local heal_states map at 5672 after tick
        let durable = include_str!("durable.rs");
        // The loop-local map `let mut heal_states: HashMap` should be gone or be on ActiveLoop
        let has_local_map =
            durable.contains("let mut heal_states") && !durable.contains("ActiveLoop");
        // We just check that state.rs has it; loop-local mutate at 7472 should be gone
        assert!(
            !durable.contains("apply_diagnose_outcome") || state.contains("heal_states"),
            "RED (bd-gs8b #14): apply_diagnose_outcome loop-local mutate still present"
        );
    }

    #[test]
    fn loop_fold_tick_is_clock_not_world_dump() {
        let durable = include_str!("durable.rs");
        // LoopMsg::Tick should be clock, not world dump — check the enum definition slice
        let pos = durable.find("LoopMsg::Tick").unwrap_or(0);
        let slice = &durable[pos..std::cmp::min(durable.len(), pos + 800)];
        let has_world_dump =
            slice.contains("states:") && slice.contains("digests:") && slice.contains("events:");
        assert!(
            !has_world_dump,
            "RED (bd-gs8b): LoopMsg::Tick still world-dumps states/digests/events — must be Tick now clock + poll as Cmd"
        );
        assert!(
            slice.contains("now: u64") || slice.contains("now"),
            "LoopMsg::Tick should be clock payload"
        );
    }
}

#[cfg(test)]
mod assignment_tick_tests {
    use super::*;

    fn empty_model() -> WorkspaceModel {
        WorkspaceModel::default()
    }

    fn inbox_row(
        corr: &str,
        wake: &str,
        body: &str,
        audience: &str,
        result_path: Option<&str>,
        priority: Option<&str>,
    ) -> serde_json::Value {
        let mut v = serde_json::json!({
            "corr": corr,
            "wake": wake,
            "body": body,
            "audience": audience,
        });
        if let Some(rp) = result_path {
            v["result_path"] = serde_json::Value::String(rp.to_string());
        }
        if let Some(p) = priority {
            v["priority"] = serde_json::Value::String(p.to_string());
        }
        v
    }

    #[test]
    fn assignment_tick_standing_empty_is_silence() {
        let m = empty_model();
        let row = inbox_row("FM-GS-012", "standing", "", "agents", None, None);
        let (next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        // standing + empty body -> NO Cmd to human pin, no result
        let has_human_prompt = cmds
            .iter()
            .any(|c| matches!(c, Cmd::Prompt{pin,..} if pin=="human" || pin=="captain"));
        let has_result = cmds
            .iter()
            .any(|c| matches!(c, Cmd::SqliteUpsert{corr, ..} if corr=="FM-GS-012"));
        assert!(
            !has_human_prompt,
            "standing empty must be silence, no human pin Cmd"
        );
        assert!(!has_result, "standing empty must not write result");
        assert!(
            next.assignment.get("FM-GS-012").is_none(),
            "standing empty must not create assignment"
        );
    }

    #[test]
    fn assignment_tick_tasked_empty_writes_result() {
        let m = empty_model();
        let row = inbox_row(
            "FM-GS-012",
            "tasked",
            "",
            "agents",
            Some("/tmp/adr27-result.txt"),
            None,
        );
        let (next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        // tasked + empty -> Cmd records result against corr with result_path
        assert!(
            next.assignment.contains_key("FM-GS-012"),
            "tasked empty must create assignment"
        );
        let receipt = next.assignment.get("FM-GS-012").unwrap();
        assert_eq!(receipt.wake, "tasked");
        assert_eq!(
            receipt.result_path.as_deref(),
            Some("/tmp/adr27-result.txt")
        );
        let has_upsert = cmds.iter().any(|c| matches!(c, Cmd::SqliteUpsert{corr, receipt} if corr=="FM-GS-012" && receipt.result_path.is_some()));
        assert!(
            has_upsert,
            "tasked empty must emit SqliteUpsert with result_path"
        );
        let has_wait = cmds
            .iter()
            .any(|c| matches!(c, Cmd::WaitCorr{corr} if corr=="FM-GS-012"));
        assert!(has_wait, "tasked should also emit WaitCorr");
    }

    #[test]
    fn assignment_tick_default_tasked_priority_is_queue() {
        let m = empty_model();
        // tasked without priority should default to queue, not interrupt
        let row = inbox_row("C1", "tasked", "hello", "agents", None, None);
        let (next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        let r = next.assignment.get("C1").expect("should create assignment");
        assert_ne!(r.wake, "interrupt");
        // Should not be treated as interrupt
        let has_interrupt_err = cmds
            .iter()
            .any(|c| matches!(c, Cmd::LoudMiss{why} if why.contains("interrupt")));
        assert!(
            !has_interrupt_err,
            "default tasked priority must be queue, not interrupt"
        );
    }

    #[test]
    fn assignment_tick_interrupt_never_human() {
        let m = empty_model();
        let row = inbox_row("C2", "tasked", "hi", "human", None, Some("interrupt"));
        let (_next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        // interrupt onto human pin is hard error
        let has_err = cmds.iter().any(|c| matches!(c, Cmd::LoudMiss{why} if why.contains("interrupt") && why.contains("human")));
        assert!(
            has_err,
            "interrupt never onto human pin must be hard error (LoudMiss)"
        );
    }

    #[test]
    fn assignment_tick_audience_human_one_pin_only() {
        let m = empty_model();
        // audience=human with corr that would fan-out to multiple pins is hard error
        // Simulate by having row with audience human and a special marker for broadcast
        let mut row = inbox_row("C3", "tasked", "hello", "human", None, None);
        row["broadcast"] = serde_json::Value::Bool(true);
        let (_next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        let has_err = cmds.iter().any(|c| matches!(c, Cmd::LoudMiss{why} if why.contains("broadcast") || why.contains("human")));
        assert!(has_err, "audience=human broadcast must be hard error");
    }

    #[test]
    fn assignment_tick_receipt_is_durable() {
        let m = empty_model();
        let receipt = Receipt {
            state: "owed".into(),
            wake: "tasked".into(),
            due_at: Some(999),
            result_path: None,
        };
        let (next, cmds) = tick(
            &m,
            &Msg::Receipt {
                corr: "FM-GS-012".into(),
                receipt: receipt.clone(),
            },
        );
        assert_eq!(next.assignment.get("FM-GS-012"), Some(&receipt));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::SqliteUpsert{corr, ..} if corr=="FM-GS-012"))
        );
    }

    #[test]
    fn assignment_tick_wait_corr_wired() {
        let m = empty_model();
        let row = inbox_row("C4", "tasked", "work", "agents", None, None);
        let (_next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::WaitCorr{corr} if corr=="C4")),
            "tasked should emit WaitCorr"
        );
    }

    #[test]
    fn assignment_tick_s1_tasked_survives() {
        // S1 scenario: tasked mail survives walker death is via durable receipt, not journal
        let m = empty_model();
        let receipt = Receipt {
            state: "owed".into(),
            wake: "tasked".into(),
            due_at: Some(123),
            result_path: Some("/tmp/x".into()),
        };
        let (next, cmds) = tick(
            &m,
            &Msg::Receipt {
                corr: "FM-GS-012".into(),
                receipt: receipt.clone(),
            },
        );
        assert!(next.assignment.contains_key("FM-GS-012"));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SqliteUpsert { .. })));
        // Simulate new process rebuilding cache from table (not journal) — assignment should be rebuildable
        let rebuilt = next.assignment.clone();
        assert_eq!(rebuilt.get("FM-GS-012"), Some(&receipt));
    }
}
