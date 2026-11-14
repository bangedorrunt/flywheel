// governed-by: ADR-0010 D10 (bd-67qg)
// governed-by: ADR-0011
//! One fleet snapshot contract (`flywheel-snapshot.v1`) + helm drain data.
//!
//! Assembled once per invocation from the workspace model (journal fold,
//! same replay rule as `doorbell why`), beads JSON, hc agent list, and
//! fallible sources that degrade to empty (never a hard error — a snapshot
//! with degraded sources is still a snapshot; `degraded` names them).
//! status / view / watch / steer_brief render from `assemble`; `helm`
//! drains the three captain sections from it.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::Config;
use crate::hc::Client;

/// Versioned schema id. Robot JSON validates on this + the top-level keys.
pub const SNAPSHOT_SCHEMA: &str = "flywheel-snapshot.v1";

/// One crew pin as the fleet sees it.
/// Wave11: SnapshotV1 agent detail — pin/npub/pane/status/verdict/bead/
/// reservations/last_active. All additive with defaults so existing doors
/// keep their JSON shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinRow {
    pub pin: String,
    pub verdict: String,
    pub source: String,
    pub since: String,
    pub bead: Option<String>,
    pub reservations: Vec<String>,
    /// Wave11 additive agent detail (defaults keep old JSON shape).
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub pending: bool,
    #[serde(default)]
    pub npub: Option<String>,
    #[serde(default)]
    pub last_active_ts_us: Option<u64>,
    #[serde(default)]
    pub identity_pin: Option<String>,
    #[serde(default)]
    pub observed_pin: Option<String>,
}

/// One bead lane.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaneRow {
    pub bead: String,
    pub title: String,
    pub status: String,
    pub priority: i64,
    pub age_secs: u64,
}

/// One open captain decision (D6 open_human fold).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DecisionRow {
    pub key: String,
    pub kind: String,
    pub opened_at: u64,
    pub waits_on: String,
    pub age_secs: u64,
    pub hold_kind: String,
}

/// Mail plane counts (receipts sqlite; degraded when unreadable).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MailCounts {
    pub owed: u64,
    pub acked: u64,
    pub resulted: u64,
    pub degraded: bool,
}

/// Journal tail state (D2 watermark fold).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JournalState {
    pub last_seq: u64,
    pub last_presented: u64,
    pub last_acked: u64,
    pub unacked_wakes: u64,
}

/// Wave11 SnapshotV1: one world read. `project`, `workspace_id`,
/// `progress`, `runs`, `fleet_workspaces`, `fleet_agents` are additive
/// (defaults) so existing doors keep their JSON shape. Doors are pure
/// projections over this struct (see status_core_counts/progress_view/
/// robot_view/serve_view).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub schema: String,
    pub generated_at: u64,
    pub workspace: String,
    pub freshness_epoch: u64,
    pub pins: Vec<PinRow>,
    pub lanes: Vec<LaneRow>,
    pub decisions: Vec<DecisionRow>,
    pub mail: MailCounts,
    pub journal: JournalState,
    pub drain_pending: usize,
    pub afk: usize,
    pub degraded: Vec<String>,
    /// ADR-0011 D5/D6: live fleet join surface
    #[serde(default)]
    pub joinable: bool,
    #[serde(default)]
    pub dispatcher: String,
    #[serde(default)]
    pub held_paths: Vec<String>,
    #[serde(default)]
    pub coalitions: Vec<String>,
    /// GS-049: agents still honestly launch-pending (bd-a2bw). Assembled from
    /// hc — never invent-green hardcoded 0 in live decide Snapshot.
    #[serde(default)]
    pub launch_pending: u32,
    /// Wave11 additive sections (defaults keep old wire shape).
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub progress: serde_json::Value,
    #[serde(default)]
    pub runs: Vec<serde_json::Value>,
    #[serde(default)]
    pub fleet_workspaces: Vec<serde_json::Value>,
    #[serde(default)]
    pub fleet_agents: Vec<serde_json::Value>,
    /// ADR-0010 D10 delivery-verdict loop projection: per-bead phase, the
    /// mail-degraded dispatch hold, queue depth, and the event watermark as
    /// last persisted by the durable loop. `loop` on the wire.
    #[serde(default, rename = "loop")]
    pub loop_state: crate::durable::LoopSnapshot,
}

/// Journal value → tick Msg (workspace first, legacy doorbell second).
/// Shared by the afk fold; same replay rule as `doorbell why`.
pub fn replay_msg(v: &serde_json::Value) -> Option<crate::workspace::Msg> {
    use crate::workspace::Msg;
    if let Ok(m) = serde_json::from_value::<Msg>(v.clone()) {
        return Some(m);
    }
    if let Ok(dm) = serde_json::from_value::<crate::doorbell_tick::DoorbellMsg>(v.clone()) {
        return Some(match dm {
            crate::doorbell_tick::DoorbellMsg::WatchNdjson { line } => Msg::WatchNdjson { line },
            crate::doorbell_tick::DoorbellMsg::InboxItems { raw, pin } => {
                Msg::InboxItems { raw, pin }
            }
            crate::doorbell_tick::DoorbellMsg::InboxRows { rows, pin } => {
                Msg::InboxRows { rows, pin }
            }
            crate::doorbell_tick::DoorbellMsg::PlaneStatus { pin, status } => {
                Msg::PlaneStatus { pin, status }
            }
            crate::doorbell_tick::DoorbellMsg::PromptRefused { pin, line } => {
                Msg::PromptRefused { pin, line }
            }
            crate::doorbell_tick::DoorbellMsg::Fact { summary } => Msg::Fact { summary },
            crate::doorbell_tick::DoorbellMsg::AckSlaTick { now } => Msg::AckSlaTick { now },
            crate::doorbell_tick::DoorbellMsg::WatchDied => Msg::WatchDied,
        });
    }
    None
}

/// Fold the doorbell journal for one workspace label into a live model.
/// Unparseable rows skip (same rule as `why`); pins restore for
/// deterministic classification.
pub fn fold_model_for(
    entries: &[(String, serde_json::Value)],
    ws_label: &str,
) -> crate::workspace::WorkspaceModel {
    let mut model = crate::workspace::WorkspaceModel::from_environment();
    for (ws, entry) in entries {
        if ws != ws_label {
            continue;
        }
        let msg_val = entry.get("msg").cloned().unwrap_or(serde_json::Value::Null);
        let Some(msg) = replay_msg(&msg_val) else {
            continue;
        };
        if let Some(pins) = entry.get("pins").and_then(|p| p.as_array()) {
            for pin in pins.iter().filter_map(|v| v.as_str()) {
                model.doorbell.index.insert_pin(pin.to_string(), None);
            }
        }
        let (next, _) = crate::workspace::tick(&model, &msg);
        model = next;
    }
    model
}

/// Bead lanes from `.beads/issues.jsonl` under the project dir.
/// Unknown shapes skip row-wise; a missing file is degraded-empty.
pub fn read_lanes(project_dir: &std::path::Path, now: u64) -> (Vec<LaneRow>, bool) {
    let path = project_dir.join(".beads/issues.jsonl");
    let Ok(body) = std::fs::read_to_string(&path) else {
        return (Vec::new(), true);
    };
    let mut lanes = Vec::new();
    for line in body.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(id) = v.get("id").and_then(|x| x.as_str()) else {
            continue;
        };
        let age_secs = v
            .get("updated_at")
            .and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| now.saturating_sub(t.timestamp() as u64))
            .unwrap_or(0);
        lanes.push(LaneRow {
            bead: id.to_string(),
            title: v
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_string(),
            priority: v.get("priority").and_then(|x| x.as_i64()).unwrap_or(0),
            age_secs,
        });
    }
    lanes.sort_by(|a, b| a.bead.cmp(&b.bead));
    (lanes, false)
}

/// Workspace pick for the snapshot pins read: first alias match wins;
/// duplicates resolve to the flywheel-owned label — the same rule as
/// `find_workspace_dyn` in main.rs. Pure so the contract is testable.
fn pick_snapshot_workspace(
    workspaces: &[crate::hc::WorkspaceInfo],
    aliases: &[String],
    owned_label: &str,
) -> Option<String> {
    let prefix = format!("{owned_label}-");
    let matches: Vec<&crate::hc::WorkspaceInfo> = workspaces
        .iter()
        .filter(|ws| aliases.iter().any(|a| a == &ws.label) || ws.label.starts_with(&prefix))
        .collect();
    if matches.is_empty() {
        return None;
    }
    if matches.len() > 1 {
        let preferred = matches
            .iter()
            .find(|ws| ws.label == owned_label || ws.label.starts_with(&prefix));
        if let Some(ws) = preferred {
            return Some(ws.workspace_id.clone());
        }
    }
    Some(matches[0].workspace_id.clone())
}

/// Fold one workspace's agents into snapshot pin rows. Shared by the hc
/// and rex doors so both report the same shape.
fn collect_pin_rows(
    agents: &[crate::hc::AgentInfo],
    ws_id: &str,
    source: &str,
    pins: &mut Vec<PinRow>,
    launch_pending: &mut u32,
) {
    for a in agents.iter().filter(|a| a.workspace_id == ws_id) {
        let pending = crate::hc::agent_still_booting(a);
        if pending {
            *launch_pending = launch_pending.saturating_add(1);
        }
        let plane_status = a.agent_status.as_str();
        let verdict = crate::workspace::verdict_for(None, plane_status, plane_status != "dead")
            .status_token();
        pins.push(PinRow {
            pin: a.name.clone().unwrap_or_else(|| a.pane_id.clone()),
            verdict,
            source: source.to_string(),
            since: String::new(),
            bead: None,
            reservations: Vec::new(),
            pane_id: a.pane_id.clone(),
            kind: a.agent.clone(),
            cwd: a.cwd.clone(),
            status: plane_status.to_string(),
            pending,
            npub: None,
            last_active_ts_us: None,
            identity_pin: None,
            observed_pin: None,
        });
    }
}

/// Assemble the SnapshotV1 world read. `now` is the freshness epoch
/// (caller clock). `ws_label` is derived internally from spawn-meta —
/// callers pass only `(cfg, project, now)`. Every fleet read door is a
/// pure projection over the returned struct; no door shells `br`/`toron`
/// separately when the snapshot is supplied.
pub fn assemble(cfg: &crate::config::Config, project: &str, now: u64) -> Snapshot {
    let ws_label_owned =
        crate::state::load_spawn_meta(project).unwrap_or_else(|| crate::swarm_label(project));
    let ws_label = ws_label_owned.as_str();
    assemble_with_label(cfg, project, ws_label, now)
}

/// Label-pinned implementation backing `assemble`. Prefer `assemble`;
/// this entry exists so TTY doors that already resolved the label keep one
/// call path (no second world read — it is the same implementation).
pub fn assemble_with_label(
    cfg: &crate::config::Config,
    project: &str,
    ws_label: &str,
    now: u64,
) -> Snapshot {
    let mut degraded = Vec::new();

    let mut pins = Vec::new();
    let mut launch_pending: u32 = 0;
    // Wave11: retain workspace_id + fleet globals from the same single
    // workspace_list/agent_list read (no second world read for robot/serve).
    let mut workspace_id = String::new();
    let mut fleet_workspaces: Vec<serde_json::Value> = Vec::new();
    let mut fleet_agents: Vec<serde_json::Value> = Vec::new();
    if crate::pane::rex_door_selected(cfg) {
        let aliases = crate::workspace_label_aliases(
            project,
            crate::state::load_spawn_meta(project).as_deref(),
        );
        let owned_label = format!("flywheel-{}", crate::project_base(project));
        match crate::backend::factory::multiplexer_backend(cfg) {
            Ok(backend) => match backend.workspace_list() {
                Ok(list) => {
                    // Wave11: fleet globals reuse the same list (no second read).
                    fleet_workspaces = list
                        .iter()
                        .filter(|w| w.label.starts_with("flywheel-"))
                        .map(|w| {
                            serde_json::json!({"workspace_id": w.workspace_id, "label": w.label, "cwd": w.cwd, "pane_count": w.pane_count})
                        })
                        .collect();
                    match pick_snapshot_workspace(&list, &aliases, &owned_label) {
                        Some(ws_id) => match backend.agent_list() {
                            Ok(agents) => {
                                let source = backend.name().to_string();
                                workspace_id = ws_id.clone();
                                fleet_agents = agents
                                .iter()
                                .map(|a| {
                                    serde_json::json!({"name": a.name, "kind": a.agent, "status": a.agent_status.as_str(), "workspace_id": a.workspace_id, "pane_id": a.pane_id})
                                })
                                .collect();
                                collect_pin_rows(
                                    &agents,
                                    &ws_id,
                                    &source,
                                    &mut pins,
                                    &mut launch_pending,
                                );
                            }
                            Err(_) => degraded.push("pins".to_string()),
                        },
                        None => degraded.push("pins".to_string()),
                    }
                }
                Err(_) => degraded.push("pins".to_string()),
            },
            Err(_) => degraded.push("pins".to_string()),
        }
    } else {
        match crate::hc::Client::connect(Some(&cfg.socket_path())) {
            Ok(mut c) => match crate::find_workspace(&mut c, project) {
                Ok(ws_id) => match c.agent_list() {
                    Ok(agents) => {
                        // Wave11: same single agent_list feeds pins + fleet.
                        workspace_id = ws_id.clone();
                        fleet_agents = agents
                            .iter()
                            .map(|a| {
                                serde_json::json!({"name": a.name, "kind": a.agent, "status": a.agent_status.as_str(), "workspace_id": a.workspace_id, "pane_id": a.pane_id})
                            })
                            .collect();
                        collect_pin_rows(&agents, &ws_id, "hc", &mut pins, &mut launch_pending);
                    }
                    Err(_) => degraded.push("pins".to_string()),
                },
                Err(_) => degraded.push("pins".to_string()),
            },
            Err(_) => degraded.push("pins".to_string()),
        }
        // Wave11: hc workspace_list for fleet globals (same read family as
        // pins; degraded-empty keeps the snapshot honest).
        match crate::hc::Client::connect(Some(&cfg.socket_path())) {
            Ok(c2) => match c2.workspace_list() {
                Ok(list) => {
                    fleet_workspaces = list
                        .iter()
                        .filter(|w| w.label.starts_with("flywheel-"))
                        .map(|w| {
                            serde_json::json!({"workspace_id": w.workspace_id, "label": w.label, "cwd": w.cwd, "pane_count": w.pane_count})
                        })
                        .collect();
                }
                Err(_) => degraded.push("fleet".to_string()),
            },
            Err(_) => degraded.push("fleet".to_string()),
        }
    }

    let project_dir: PathBuf = crate::project_dir(cfg, project);
    let (lanes, lanes_degraded) = read_lanes(&project_dir, now);
    if lanes_degraded {
        degraded.push("lanes".to_string());
    }

    let journal_entries = crate::doorbell::journal_read_all(&crate::config::state_dir());
    let model = fold_model_for(&journal_entries, ws_label);
    let decisions: Vec<DecisionRow> = crate::workspace::open_decisions(&model)
        .into_iter()
        .map(|d| DecisionRow {
            age_secs: now.saturating_sub(d.created_at),
            key: d.corr,
            kind: "corr".to_string(),
            opened_at: d.created_at,
            waits_on: d.body,
            hold_kind: "open".to_string(),
        })
        .collect();
    let last_presented = model
        .wake
        .shown
        .values()
        .flatten()
        .map(|e| e.seq)
        .max()
        .unwrap_or(0);
    let last_acked = model.wake_acked.values().copied().max().unwrap_or(0);
    let unacked_wakes = model
        .wake
        .shown
        .iter()
        .map(|(pin, es)| {
            let wm = model.wake_acked.get(pin).copied().unwrap_or(0);
            es.iter().filter(|e| e.seq > wm).count() as u64
        })
        .sum();
    let journal = JournalState {
        last_seq: model.seq,
        last_presented,
        last_acked,
        unacked_wakes,
    };
    let drain_pending = model.wake.waits.len();
    let afk = model.wake.digests.len();

    let (mail, mail_degraded) = read_mail_counts(project);
    if mail_degraded {
        degraded.push("mail".to_string());
    }
    let (board, res_degraded) = read_reservation_board(cfg, project);
    if res_degraded {
        degraded.push("reservations".to_string());
    }
    let held_paths = board.as_ref().map(parse_held_paths).unwrap_or_default();
    if let Some(board) = &board {
        let identities: std::collections::BTreeMap<String, String> =
            crate::state::load_spawn_registry(&project_dir.to_string_lossy())
                .into_iter()
                .filter_map(|e| e.mail_identity.map(|id| (e.name, id)))
                .collect();
        bind_pins_to_board(&mut pins, board, &|name| identities.get(name).cloned());
    }

    let state_dir = crate::config::state_dir();
    let token = crate::doorbell::dispatcher_token(&state_dir, ws_label);
    let dispatcher = if token == "absent" {
        crate::doorbell::dispatcher_token_for_project(&state_dir, project)
    } else {
        token
    }
    .to_string();
    let coalitions = list_active_coalitions(project, now);
    let joinable = fleet_joinable(&dispatcher, &pins);

    let loop_state = match crate::state::load_active_loop(project) {
        Ok(Some(al)) => crate::durable::LoopSnapshot::from_active_loop(&al),
        Ok(None) => crate::durable::LoopSnapshot::default(),
        Err(_) => {
            degraded.push("loop".to_string());
            crate::durable::LoopSnapshot::default()
        }
    };

    // Wave11: beads progress + runs in the same single read (degraded-empty
    // on failure — never a hard error). `progress_from_rows` is the pure
    // core shared with `cmd_progress`; `snapshot_runs` is the runs section.
    let (progress, progress_degraded) = read_progress_section(cfg, project);
    if progress_degraded {
        degraded.push("progress".to_string());
    }
    let runs = snapshot_runs();

    Snapshot {
        schema: SNAPSHOT_SCHEMA.to_string(),
        generated_at: now,
        workspace: ws_label.to_string(),
        freshness_epoch: now,
        pins,
        lanes,
        decisions,
        mail,
        journal,
        drain_pending,
        afk,
        degraded,
        joinable,
        dispatcher,
        held_paths,
        coalitions,
        launch_pending,
        loop_state,
        // Wave11 additive sections: single read above; defaults only when
        // the source degraded (never a hard error).
        project: project.to_string(),
        workspace_id,
        progress,
        runs,
        fleet_workspaces,
        fleet_agents,
    }
}

/// Wave11: beads-progress section reader (br list + ready). Pure core is
/// `progress_from_rows`; this wrapper only performs the two `br` reads.
/// Returns `(progress_value, degraded)` — degraded-empty keeps the snapshot
/// honest (never invent-green zero beads).
fn read_progress_section(cfg: &crate::config::Config, project: &str) -> (serde_json::Value, bool) {
    let dir = crate::project_dir(cfg, project);
    let rows = match crate::br::Bead::list_issues(Some(&dir), &["--status", "all"], "br list") {
        Ok(r) => r,
        Err(_) => {
            return (
                serde_json::json!({"dispatching_wave": serde_json::Value::Null, "waves": [], "unlabeled": {"closed": 0, "in_progress": 0, "ready": 0, "blocked": 0, "unknown": 0, "total": 0}}),
                true,
            );
        }
    };
    let ready_ids: std::collections::HashSet<String> = match crate::br::run_br(
        Some(&dir),
        &["ready", "--json"],
    ) {
        Ok(raw) => match crate::br::parse_ready_beads_json(&raw) {
            Ok(beads) => beads.into_iter().map(|b| b.id).collect(),
            Err(_) => {
                return (
                    serde_json::json!({"dispatching_wave": serde_json::Value::Null, "waves": [], "unlabeled": {"closed": 0, "in_progress": 0, "ready": 0, "blocked": 0, "unknown": 0, "total": 0}}),
                    true,
                );
            }
        },
        Err(_) => {
            return (
                serde_json::json!({"dispatching_wave": serde_json::Value::Null, "waves": [], "unlabeled": {"closed": 0, "in_progress": 0, "ready": 0, "blocked": 0, "unknown": 0, "total": 0}}),
                true,
            );
        }
    };
    (progress_from_rows(&rows, &ready_ids), false)
}

fn read_mail_counts(_project: &str) -> (MailCounts, bool) {
    let out = std::process::Command::new("toron")
        .args(["receipt", "list", "--json"])
        .output();
    let Ok(out) = out else {
        return (
            MailCounts {
                owed: 0,
                acked: 0,
                resulted: 0,
                degraded: true,
            },
            true,
        );
    };
    if !out.status.success() {
        return (
            MailCounts {
                owed: 0,
                acked: 0,
                resulted: 0,
                degraded: true,
            },
            true,
        );
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return (
            MailCounts {
                owed: 0,
                acked: 0,
                resulted: 0,
                degraded: true,
            },
            true,
        );
    };
    let rows = v.as_array().cloned().unwrap_or_default();
    let mut owed = 0u64;
    let mut acked = 0u64;
    let mut resulted = 0u64;
    for r in &rows {
        match r.get("state").and_then(|s| s.as_str()).unwrap_or("") {
            "owed" => owed += 1,
            "acked" => acked += 1,
            "resulted" => resulted += 1,
            _ => {}
        }
    }
    (
        MailCounts {
            owed,
            acked,
            resulted,
            degraded: false,
        },
        false,
    )
}

fn parse_held_paths(board: &serde_json::Value) -> Vec<String> {
    board
        .get("reservations")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r.get("path").and_then(|p| p.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// bd-swarm-cockpit-pczs: ONE live board read feeds `held_paths` AND the
/// per-pin bead/reservation binding below. `Ok(None)` on a miss (the caller
/// degrades loudly, never fabricating a calm board).
fn read_reservation_board(
    cfg: &crate::config::Config,
    project: &str,
) -> (Option<serde_json::Value>, bool) {
    match crate::mail::Mail::from_config(Some(&cfg.toron_mail_config())) {
        Ok(Some(mail)) => match mail.list_active(project, None, None) {
            Ok(v) => (Some(v), false),
            Err(_) => (None, true),
        },
        _ => (None, true),
    }
}

/// bd-swarm-cockpit-pczs: bind each pane to its current bead claim and held
/// reservations from the LIVE reservation board. A row's `reason` is the
/// reserve-before-work lifecycle's bead id (the same rule `claim_watch_step`
/// consults), and the row's `holder` is the resolved actor: a pane matches
/// when the holder equals its terminal name OR the mail identity the caller
/// resolves for that name (leases bind the MAIL pin, not the pane title). A
/// pin with no row keeps empty bead/reservations — absence, never a default.
/// Pure over the parsed board and an injected identity lookup.
pub fn bind_pins_to_board(
    pins: &mut [PinRow],
    board: &serde_json::Value,
    identity_of: &dyn Fn(&str) -> Option<String>,
) {
    let Some(rows) = board.get("reservations").and_then(|r| r.as_array()) else {
        return;
    };
    for pin in pins.iter_mut() {
        let mail_pin = identity_of(&pin.pin);
        for row in rows {
            let holder = row.get("holder").and_then(|h| h.as_str()).unwrap_or("");
            let matches = holder == pin.pin || mail_pin.as_deref() == Some(holder);
            if !matches {
                continue;
            }
            if let Some(path) = row.get("path").and_then(|p| p.as_str()) {
                let path = path.to_string();
                if !pin.reservations.contains(&path) {
                    pin.reservations.push(path);
                }
            }
            if pin.bead.is_none() {
                let reason = row.get("reason").and_then(|r| r.as_str()).unwrap_or("");
                if let Some(bead) = reason
                    .split_whitespace()
                    .next()
                    .filter(|s| s.starts_with("bd-"))
                {
                    pin.bead = Some(bead.to_string());
                }
            }
        }
    }
}

/// hc terminal ids look like `wP:p1` / `w6:p2`. A lone current pane is not a fleet.
/// Compatibility wrappers for pure snapshot reporting predicates.
#[allow(dead_code)]
pub fn looks_like_pane_id(pin: &str) -> bool {
    crate::snapshot_reporting::looks_like_pane_id(pin)
}

pub fn fleet_joinable(dispatcher: &str, pins: &[PinRow]) -> bool {
    crate::snapshot_reporting::fleet_joinable(dispatcher, pins)
}

#[allow(dead_code)]
pub const COALITION_LIVE_SECS: u64 = crate::snapshot_reporting::COALITION_LIVE_SECS;

pub fn coalition_is_live(created_at: &str, now_unix: u64) -> bool {
    crate::snapshot_reporting::coalition_is_live(created_at, now_unix)
}

fn list_active_coalitions(_project: &str, now_unix: u64) -> Vec<String> {
    let root = crate::config::state_dir().join("coalitions");
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        if v.get("status").and_then(|s| s.as_str()) != Some("active") {
            continue;
        }
        let created = v.get("created_at").and_then(|s| s.as_str()).unwrap_or("");
        if !coalition_is_live(created, now_unix) {
            continue;
        }
        if let Some(id) = v.get("id").and_then(|s| s.as_str()) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Compatibility wrapper for the pure snapshot reporting seam.
pub fn bearings_line(snap: &Snapshot) -> String {
    crate::snapshot_reporting::bearings_line(snap)
}

/// Helm drain: always the three sections, empty-state sentences allowed.
pub fn render_helm(snap: &Snapshot, now: u64) -> String {
    let mut out = String::from("OPEN DECISIONS");
    if snap.decisions.is_empty() {
        out.push_str(": none — no open captain calls");
    } else {
        for d in &snap.decisions {
            out.push_str(&format!(
                "\n- [{}] {} (opened {}s ago, waits on: {})",
                d.key,
                d.hold_kind,
                now.saturating_sub(d.opened_at),
                d.waits_on
            ));
        }
    }
    out.push_str("\nUNREAD STATUS");
    let unread_bits: Vec<String> = {
        let mut v = Vec::new();
        if snap.mail.owed > 0 {
            v.push(format!("{} mail owed", snap.mail.owed));
        }
        if snap.journal.unacked_wakes > 0 {
            v.push(format!("{} unacked wakes", snap.journal.unacked_wakes));
        }
        if snap.afk > 0 {
            v.push(format!("{} afk queued", snap.afk));
        }
        if snap.loop_state.mail_degraded {
            v.push("mail-degraded dispatch hold".to_string());
        }
        v
    };
    if unread_bits.is_empty() {
        out.push_str(": all read — no mail owed, no unacked wakes, digest empty");
    } else {
        out.push_str(&format!(": {}", unread_bits.join(", ")));
    }
    out.push_str("\nRECORD DIVERGENCE");
    let open_corrs: Vec<(String, String)> = snap
        .decisions
        .iter()
        .map(|d| (d.key.clone(), d.waits_on.clone()))
        .collect();
    let closed_beads: Vec<String> = snap
        .lanes
        .iter()
        .filter(|l| l.status == "closed")
        .map(|l| l.bead.clone())
        .collect();
    let spec = crate::workspace::DivergenceSpec {
        open_corrs: &open_corrs,
        closed_beads: &closed_beads,
        answered_corrs: &[],
        held_beads: &[],
        done_without_close: &[],
    };
    let rows = crate::workspace::record_divergence(&spec);
    if rows.is_empty() {
        out.push_str(": none");
    } else {
        out.push_str(&format!("\n{}", crate::workspace::render_divergence(&rows)));
    }
    out.push_str("\nFLEET");
    if snap.joinable {
        out.push_str(&format!(
            ": JOINING — not spawning  dispatcher={} pins={} coalitions={}",
            snap.dispatcher,
            snap.pins.len(),
            snap.coalitions.len()
        ));
    } else {
        out.push_str(&format!(": none live (dispatcher={})", snap.dispatcher));
    }
    out.push('\n');
    out
}

pub(crate) fn robot_snapshot(cfg: &Config) -> Result<Value> {
    if crate::pane::rex_door_selected(cfg) {
        let backend = crate::backend::multiplexer_backend(cfg)?;
        let workspaces = backend.workspace_list()?;
        let agents = backend.agent_list()?;
        let fly: Vec<Value> = workspaces
            .iter()
            .filter(|w| w.label.starts_with("flywheel-"))
            .map(|w| {
                let ws_agents: Vec<Value> = agents
                    .iter()
                    .filter(|a| a.workspace_id == w.workspace_id)
                    .map(|a| {
                        json!({
                            "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
                            "pane_id": a.pane_id, "cwd": a.cwd, "interactive_ready": a.interactive_ready
                        })
                    })
                    .collect();
                json!({
                    "workspace_id": w.workspace_id,
                    "label": w.label,
                    "cwd": w.cwd,
                    "pane_count": w.pane_count,
                    "agents": ws_agents
                })
            })
            .collect();
        let runs: Vec<Value> = snapshot_runs();
        return Ok(json!({
            "tool": "flywheel",
            "version": crate::VERSION,
            "ts": crate::state::now_iso(),
            "workspaces": fly,
            "runs": runs,
            "agents": agents.iter().map(|a| json!({
                "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
                "workspace_id": a.workspace_id, "pane_id": a.pane_id
            })).collect::<Vec<_>>()
        }));
    }
    let c = Client::connect(Some(&cfg.socket_path()))?;
    let workspaces = c.workspace_list()?;
    let agents = c.agent_list()?;
    let fly: Vec<Value> = workspaces
        .iter()
        .filter(|w| w.label.starts_with("flywheel-"))
        .map(|w| {
            let ws_agents: Vec<Value> = agents
                .iter()
                .filter(|a| a.workspace_id == w.workspace_id)
                .map(|a| {
                    json!({
                        "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
                        "pane_id": a.pane_id, "cwd": a.cwd, "interactive_ready": a.interactive_ready
                    })
                })
                .collect();
            json!({
                "workspace_id": w.workspace_id,
                "label": w.label,
                "cwd": w.cwd,
                "pane_count": w.pane_count,
                "agents": ws_agents
            })
        })
        .collect();
    let runs: Vec<Value> = snapshot_runs();
    Ok(json!({
        "tool": "flywheel",
        "version": crate::VERSION,
        "ts": crate::state::now_iso(),
        "workspaces": fly,
        "runs": runs,
        "agents": agents.iter().map(|a| json!({
            "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
            "workspace_id": a.workspace_id, "pane_id": a.pane_id
        })).collect::<Vec<_>>()
    }))
}

/// Run list for the robot snapshot (P6.4) — workflow/pipeline/loop entries
/// from the per-project logs, newest first, with state + owner + run-id.
/// Independent of the `runs` CLI so the snapshot has no subcommand coupling.
pub(crate) fn snapshot_runs() -> Vec<Value> {
    use crate::state::EventKind;
    let mut entries = Vec::new();
    for project in crate::state::project_logs() {
        let Ok(rows) = crate::state::read_log(&project) else {
            continue;
        };
        for entry in rows {
            if matches!(
                entry.kind,
                EventKind::Workflow | EventKind::Pipeline | EventKind::Loop
            ) {
                entries.push((project.clone(), entry));
            }
        }
    }

    latest_run_entries(entries)
        .into_iter()
        .map(|(project, entry)| {
            let run_kind = crate::run_row_kind(&project, &entry);
            let owner = ["workflow", "pipeline", "loop", "agent"]
                .iter()
                .find_map(|key| {
                    entry
                        .data
                        .get(*key)
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .unwrap_or_default();
            json!({
                "id": format!("{project}:{}", entry.ts),
                "project": project,
                "kind": run_kind,
                "owner": owner,
                "run_id": entry.data.get("run_id").cloned().unwrap_or(Value::Null),
                "state": crate::history_row_status(&entry.data),
                "ts": entry.ts,
            })
        })
        .collect()
}

/// Reconcile lifecycle transitions to the latest outcome for each logical run.
///
/// A start row and its later terminal row share `run_id`; treating both as
/// current makes a completed run look stuck. Annotation rows are not outcomes
/// and rows without a `run_id` cannot be safely correlated, so both remain
/// visible. This is deliberately a read projection: it never edits history or
/// turns an unresolved run into a guessed terminal state.
pub(crate) fn latest_run_entries(
    mut entries: Vec<(String, crate::state::LogEntry)>,
) -> Vec<(String, crate::state::LogEntry)> {
    entries.sort_by(|a, b| b.1.ts.cmp(&a.1.ts));
    let mut seen = std::collections::BTreeSet::new();
    entries
        .into_iter()
        .filter(|(project, entry)| {
            if crate::is_annotation_row(entry) {
                return true;
            }
            let Some(run_id) = entry.data.get("run_id").and_then(Value::as_str) else {
                return true;
            };
            seen.insert((project.clone(), run_id.to_string()))
        })
        .collect()
}

/// Top-level keys the robot JSON must carry (schema validation).
/// Wave11 SnapshotV1: project/agents-detail/progress/runs/fleet are
/// additive — existing doors ignore unknown keys.
#[cfg(test)]
pub fn schema_keys() -> Vec<&'static str> {
    vec![
        "schema",
        "generated_at",
        "workspace",
        "freshness_epoch",
        "pins",
        "lanes",
        "decisions",
        "mail",
        "journal",
        "drain_pending",
        "afk",
        "degraded",
        "joinable",
        "dispatcher",
        "held_paths",
        "coalitions",
        "launch_pending",
        "loop",
        "project",
        "workspace_id",
        "progress",
        "runs",
        "fleet_workspaces",
        "fleet_agents",
    ]
}

/// Wave11 SnapshotV1 pure projections — no I/O, no `br`/`toron` shells.
/// Every fleet read door renders through these from one `assemble`.
/// Core-facts agreement (agent/bead/reservation counts) is asserted by the
/// wave11 fixture test in `tests/adr0010_d10.rs`.
///
/// `progress_from_rows` mirrors `cmd::status::progress_waves` without
/// importing it (snapshot cannot depend on cmd::status — one direction only).
/// Typed `wave` field wins, legacy `wave-N` label is fallback, else
/// `unlabeled`; dispatching wave is the lowest wave holding ready/in-progress.
pub fn progress_from_rows(
    rows: &[serde_json::Value],
    ready_ids: &std::collections::HashSet<String>,
) -> serde_json::Value {
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct C {
        closed: u64,
        in_progress: u64,
        ready: u64,
        blocked: u64,
        unknown: u64,
        total: u64,
    }
    impl C {
        fn add(&mut self, status: &str, ready: bool) {
            match status {
                "closed" => self.closed += 1,
                "in_progress" => self.in_progress += 1,
                "open" if ready => self.ready += 1,
                "open" => self.blocked += 1,
                _ => self.unknown += 1,
            }
            self.total += 1;
        }
        fn to_json(&self) -> serde_json::Value {
            serde_json::json!({"closed": self.closed, "in_progress": self.in_progress, "ready": self.ready, "blocked": self.blocked, "unknown": self.unknown, "total": self.total})
        }
    }
    let mut waves: BTreeMap<u64, C> = BTreeMap::new();
    let mut unlabeled = C::default();
    for row in rows {
        let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let wave = row.get("wave").and_then(|v| v.as_u64()).or_else(|| {
            row.get("labels")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .find_map(|l| l.strip_prefix("wave-").and_then(|n| n.parse::<u64>().ok()))
        });
        match wave {
            Some(w) => waves
                .entry(w)
                .or_default()
                .add(status, ready_ids.contains(id)),
            None => unlabeled.add(status, ready_ids.contains(id)),
        }
    }
    let dispatching = waves
        .iter()
        .find(|(_, c)| c.in_progress > 0 || c.ready > 0)
        .map(|(w, _)| *w);
    serde_json::json!({
        "dispatching_wave": dispatching,
        "waves": waves.iter().map(|(w, c)| { let mut v = c.to_json(); v["wave"] = serde_json::json!(w); v }).collect::<Vec<_>>(),
        "unlabeled": unlabeled.to_json(),
    })
}

/// Core-facts agreement: agent count, bead counts by status, reservation
/// count. Pure over `&Snapshot` — doors must agree on these.
/// Wave11 SnapshotV1 pure projections — no I/O, no `br`/`toron` shells.
/// `progress_from_rows`/`progress_view` are live (assemble + progress door).
/// `status_core_counts`/`robot_view`/`serve_view` are pure cores proven by
/// the wave11 fixture; live global doors keep current shape (residual in
/// `.flywheel/reports/wave11-snapshot.md`), hence allow(dead_code) until
/// per-project scoping lands — never a second world read.
#[allow(dead_code)]
pub fn status_core_counts(snap: &Snapshot) -> serde_json::Value {
    use std::collections::BTreeMap;
    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    for l in &snap.lanes {
        *by_status.entry(l.status.clone()).or_default() += 1;
    }
    serde_json::json!({
        "agents": snap.pins.len(),
        "beads_by_status": by_status,
        "beads_total": snap.lanes.len(),
        "reservations": snap.held_paths.len(),
    })
}

/// `flywheel progress` projection — same shape as `cmd_progress` JSON
/// (`project` + waves + unlabeled + dispatching), sourced from the snapshot.
/// Wave11: `progress` door projection (live). See module docs for residual.
#[allow(dead_code)]
pub fn progress_view(snap: &Snapshot) -> serde_json::Value {
    let mut out = snap.progress.clone();
    if out.is_null() {
        out = serde_json::json!({"dispatching_wave": serde_json::Value::Null, "waves": [], "unlabeled": {"closed": 0, "in_progress": 0, "ready": 0, "blocked": 0, "unknown": 0, "total": 0}});
    }
    out["project"] = serde_json::json!(snap.project.clone());
    out
}

/// `--robot-snapshot` projection — same top-level keys as `robot_snapshot`
/// (`tool/version/ts/workspaces/runs/agents`), scoped to the snapshot's
/// workspace. The global door reuses the same fleet helpers (no second
/// world-read shape); the fixture asserts core counts agree.
/// Wave11: `--robot-snapshot` pure core (residual: global fleet scoping).
#[allow(dead_code)]
pub fn robot_view(snap: &Snapshot) -> serde_json::Value {
    let agents: Vec<serde_json::Value> = snap.pins.iter().map(|p| {
        serde_json::json!({"name": p.pin, "kind": p.kind, "status": p.status, "pane_id": p.pane_id, "workspace_id": snap.workspace_id})
    }).collect();
    serde_json::json!({
        "tool": "flywheel",
        "version": crate::VERSION,
        "ts": crate::state::now_iso(),
        "workspaces": [serde_json::json!({"label": snap.workspace, "workspace_id": snap.workspace_id, "agents": agents})],
        "runs": snap.runs,
        "agents": agents,
        "core": status_core_counts(snap),
    })
}

/// `serve /api/v1/snapshot` projection — agent list scoped to the snapshot.
/// Wave11: `serve /api/v1/snapshot` pure core (residual: global scoping).
#[allow(dead_code)]
pub fn serve_view(snap: &Snapshot) -> serde_json::Value {
    serde_json::json!({
        "agents": snap.pins.iter().map(|p| {
            serde_json::json!({"name": p.pin, "kind": p.kind, "status": p.status, "workspace_id": snap.workspace_id, "pane_id": p.pane_id})
        }).collect::<Vec<_>>(),
        "core": status_core_counts(snap),
    })
}

#[cfg(test)]
mod adr0010_d10_tests {
    // governed-by: ADR-0010 D10 (bd-67qg).
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            schema: SNAPSHOT_SCHEMA.to_string(),
            generated_at: 1000,
            workspace: "w".to_string(),
            freshness_epoch: 1000,
            pins: vec![PinRow {
                pin: "P".into(),
                verdict: "Idle(hc)".into(),
                source: "hc".into(),
                since: String::new(),
                bead: None,
                reservations: vec![],
                pane_id: String::new(),
                kind: None,
                cwd: None,
                status: String::new(),
                pending: false,
                npub: None,
                last_active_ts_us: None,
                identity_pin: None,
                observed_pin: None,
            }],
            lanes: vec![LaneRow {
                bead: "bd-1".into(),
                title: "t".into(),
                status: "in_progress".into(),
                priority: 1,
                age_secs: 7,
            }],
            decisions: vec![DecisionRow {
                key: "c-1".into(),
                kind: "corr".into(),
                opened_at: 900,
                waits_on: "approve bd-1".into(),
                age_secs: 100,
                hold_kind: "open".into(),
            }],
            mail: MailCounts {
                owed: 0,
                acked: 2,
                resulted: 1,
                degraded: false,
            },
            journal: JournalState {
                last_seq: 9,
                last_presented: 8,
                last_acked: 8,
                unacked_wakes: 0,
            },
            drain_pending: 0,
            afk: 0,
            degraded: vec![],
            joinable: false,
            dispatcher: "absent".into(),
            held_paths: vec![],
            coalitions: vec![],
            launch_pending: 0,
            loop_state: crate::durable::LoopSnapshot::default(),
            project: String::new(),
            workspace_id: String::new(),
            progress: serde_json::Value::Null,
            runs: Vec::new(),
            fleet_workspaces: Vec::new(),
            fleet_agents: Vec::new(),
        }
    }

    #[test]
    fn adr0010_d10_robot_json_validates_against_schema() {
        let v = serde_json::to_value(sample()).expect("snapshot serializes");
        assert_eq!(
            v.get("schema").and_then(|s| s.as_str()),
            Some(SNAPSHOT_SCHEMA)
        );
        for key in schema_keys() {
            assert!(v.get(key).is_some(), "robot JSON missing key {key}");
        }
    }

    #[test]
    fn pane_id_is_not_a_named_pin() {
        assert!(looks_like_pane_id("wP:p1"));
        assert!(looks_like_pane_id("w6:p2"));
        assert!(!looks_like_pane_id("LucidFalcon"));
        assert!(!looks_like_pane_id("TealDune"));
    }

    #[test]
    fn lone_plane_pane_is_not_joinable() {
        let pins = vec![PinRow {
            pin: "wP:p1".into(),
            verdict: "Busy(hc-evidence)".into(),
            source: "hc".into(),
            since: String::new(),
            bead: None,
            reservations: vec![],
            pane_id: String::new(),
            kind: None,
            cwd: None,
            status: String::new(),
            pending: false,
            npub: None,
            last_active_ts_us: None,
            identity_pin: None,
            observed_pin: None,
        }];
        assert!(!fleet_joinable("absent", &pins));
        assert!(fleet_joinable("up", &pins));
        let named = vec![PinRow {
            pin: "LucidFalcon".into(),
            verdict: "Idle(hc)".into(),
            source: "hc".into(),
            since: String::new(),
            bead: None,
            reservations: vec![],
            pane_id: String::new(),
            kind: None,
            cwd: None,
            status: String::new(),
            pending: false,
            npub: None,
            last_active_ts_us: None,
            identity_pin: None,
            observed_pin: None,
        }];
        assert!(fleet_joinable("absent", &named));
    }

    #[test]
    fn adr0010_d10_helm_empty_queue_prints_three_sections() {
        let mut s = sample();
        s.decisions.clear();
        s.lanes.clear();
        let text = render_helm(&s, 1000);
        assert!(text.contains("OPEN DECISIONS: none"), "{text}");
        assert!(text.contains("UNREAD STATUS: all read"), "{text}");
        assert!(text.contains("RECORD DIVERGENCE: none"), "{text}");
    }

    #[test]
    fn adr0010_d10_helm_full_drain_shape() {
        let text = render_helm(&sample(), 1000);
        assert!(
            text.contains("OPEN DECISIONS\n- [c-1] open (opened 100s ago"),
            "{text}"
        );
        assert!(text.contains("UNREAD STATUS: all read"), "{text}");
        assert!(
            text.contains("br show bd-1") || text.contains("RECORD DIVERGENCE"),
            "{text}"
        );
        assert!(text.contains("RECORD DIVERGENCE"), "{text}");
    }

    #[test]
    fn adr0010_d10_bearings_line_marks_degraded() {
        let mut s = sample();
        s.degraded.push("mail".to_string());
        let line = bearings_line(&s);
        assert!(line.contains("1 pins, 1 open decisions"), "{line}");
        assert!(line.contains("degraded: mail"), "{line}");
    }

    #[test]
    fn adr0010_d10_read_lanes_skips_bad_rows() {
        let dir = std::env::temp_dir().join("flywheel-d10-test-lanes");
        std::fs::create_dir_all(dir.join(".beads")).expect("mkdir");
        std::fs::write(
            dir.join(".beads/issues.jsonl"),
            "{\"id\":\"bd-9\",\"title\":\"t\",\"status\":\"open\",\"priority\":2}\nnot json\n{\"no-id\":1}\n",
        )
        .expect("write");
        let (lanes, degraded) = read_lanes(&dir, 500);
        assert!(!degraded);
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].bead, "bd-9");
        assert_eq!(lanes[0].priority, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_held_paths_from_board() {
        let board = serde_json::json!({
            "project": "toron",
            "count": 1,
            "reservations": [
                {"id": "abc", "path": "src/**", "holder": "LucidFalcon"}
            ]
        });
        assert_eq!(parse_held_paths(&board), vec!["src/**"]);
        assert_eq!(parse_held_paths(&serde_json::json!({})), [] as [String; 0]);
    }

    #[test]
    fn month_old_coalition_is_not_live() {
        assert!(!coalition_is_live("2026-08-10T00:28:26Z", 1_788_840_000));
        assert!(coalition_is_live("2026-09-07T12:00:00Z", 1_788_840_000));
        assert!(!coalition_is_live("not-a-date", 1_788_840_000));
    }

    /// bd-swarm-cockpit-pczs Acceptance: a pane's bead claim and held
    /// reservations come from the LIVE reservation board — a row's `reason`
    /// is the reserve-before-work bead id (the rule `claim_watch_step`
    /// consults), and it matches the pane either by its terminal name or by the
    /// mail identity the caller resolves for it (leases bind the MAIL pin,
    /// not the pane title). A pane with no row stays empty: absence, never a
    /// fabricated claim.
    #[test]
    fn status_binds_pane_to_bead() {
        let mut pins = vec![
            PinRow {
                pin: "crew-1-amberelk".into(),
                verdict: "Busy(hc-evidence)".into(),
                source: "hc".into(),
                since: String::new(),
                bead: None,
                reservations: vec![],
                pane_id: String::new(),
                kind: None,
                cwd: None,
                status: String::new(),
                pending: false,
                npub: None,
                last_active_ts_us: None,
                identity_pin: None,
                observed_pin: None,
            },
            PinRow {
                pin: "wP:p9".into(),
                verdict: "Idle(hc)".into(),
                source: "hc".into(),
                since: String::new(),
                bead: None,
                reservations: vec![],
                pane_id: String::new(),
                kind: None,
                cwd: None,
                status: String::new(),
                pending: false,
                npub: None,
                last_active_ts_us: None,
                identity_pin: None,
                observed_pin: None,
            },
        ];
        let board = serde_json::json!({
            "count": 3,
            "reservations": [
                {"path": "src/crew.rs", "holder": "AmberElk", "reason": "bd-zgi9"},
                {"path": "CHANGELOG.md", "holder": "AmberElk", "reason": "bd-zgi9"},
                {"path": "docs/x.md", "holder": "wP:p9", "reason": ""}
            ]
        });
        let identities = |name: &str| (name == "crew-1-amberelk").then(|| "AmberElk".to_string());
        bind_pins_to_board(&mut pins, &board, &identities);
        assert_eq!(pins[0].bead.as_deref(), Some("bd-zgi9"));
        assert_eq!(pins[0].reservations, vec!["src/crew.rs", "CHANGELOG.md"]);
        assert_eq!(pins[1].bead, None);
        assert_eq!(pins[1].reservations, vec!["docs/x.md"]);
        bind_pins_to_board(&mut pins, &board, &identities);
        assert_eq!(
            pins[0].reservations.len(),
            2,
            "no duplicate rows on re-bind"
        );
        bind_pins_to_board(&mut pins, &serde_json::json!({}), &identities);
        assert_eq!(pins[0].bead.as_deref(), Some("bd-zgi9"));
    }

    fn ws(id: &str, label: &str) -> crate::hc::WorkspaceInfo {
        crate::hc::WorkspaceInfo {
            workspace_id: id.to_string(),
            number: 0,
            label: label.to_string(),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: String::new(),
            agent_status: crate::hc::AgentStatus::Unknown,
            cwd: None,
        }
    }

    #[test]
    fn snapshot_workspace_pick_prefers_owned_label() {
        let list = vec![ws("w-stale", "proj"), ws("w-live", "flywheel-proj")];
        let aliases = vec!["flywheel-proj".to_string(), "proj".to_string()];
        assert_eq!(
            pick_snapshot_workspace(&list, &aliases, "flywheel-proj").as_deref(),
            Some("w-live")
        );
    }

    #[test]
    fn snapshot_workspace_pick_single_match_and_miss() {
        let list = vec![ws("w1", "proj")];
        let aliases = vec!["proj".to_string()];
        assert_eq!(
            pick_snapshot_workspace(&list, &aliases, "flywheel-proj").as_deref(),
            Some("w1")
        );
        let none: Vec<String> = vec![];
        assert_eq!(pick_snapshot_workspace(&list, &none, "flywheel-proj"), None);
    }

    #[test]
    fn snapshot_workspace_pick_matches_keyed_workspace() {
        let list = vec![ws("w-keyed", "flywheel-proj-rk-loop-1")];
        let aliases = vec!["flywheel-proj".to_string(), "proj".to_string()];
        assert_eq!(
            pick_snapshot_workspace(&list, &aliases, "flywheel-proj").as_deref(),
            Some("w-keyed")
        );
    }

    /// Wave11 (bd-wave11-snapshot-mndgf): fixture with 2 agents / 2 beads /
    /// 1 reservation — every pure projection agrees on core facts, and no
    /// projection shells `br`/`toron` (each takes `&Snapshot` only).
    #[test]
    fn wave11_projections_agree_on_core_facts() {
        use std::collections::HashSet;
        fn pin(pin: &str) -> PinRow {
            PinRow {
                pin: pin.to_string(),
                verdict: "Idle(hc)".to_string(),
                source: "hc".to_string(),
                since: String::new(),
                bead: None,
                reservations: vec![],
                pane_id: format!("{pin}-p1"),
                kind: Some("pi".to_string()),
                cwd: None,
                status: "idle".to_string(),
                pending: false,
                npub: None,
                last_active_ts_us: None,
                identity_pin: None,
                observed_pin: None,
            }
        }
        let rows = serde_json::json!([
            {"id": "bd-a", "title": "lane-a", "status": "open", "priority": 2, "wave": 1},
            {"id": "bd-b", "title": "lane-b", "status": "closed", "priority": 1, "wave": 1}
        ]);
        let rows_vec = rows.as_array().unwrap().clone();
        let ready: HashSet<String> = ["bd-a".to_string()].into_iter().collect();
        let progress = progress_from_rows(&rows_vec, &ready);
        assert_eq!(progress["dispatching_wave"], serde_json::json!(1));
        let mut pins = vec![pin("QuietBay"), pin("AmberHarbor")];
        pins[0].bead = Some("bd-a".to_string());
        pins[0].reservations = vec!["src/snapshot.rs".to_string()];
        let snap = Snapshot {
            schema: SNAPSHOT_SCHEMA.to_string(),
            generated_at: 1000,
            workspace: "flywheel-flywheel".to_string(),
            freshness_epoch: 1000,
            pins,
            lanes: vec![
                LaneRow {
                    bead: "bd-a".to_string(),
                    title: "lane-a".to_string(),
                    status: "open".to_string(),
                    priority: 2,
                    age_secs: 1,
                },
                LaneRow {
                    bead: "bd-b".to_string(),
                    title: "lane-b".to_string(),
                    status: "closed".to_string(),
                    priority: 1,
                    age_secs: 2,
                },
            ],
            decisions: vec![],
            mail: MailCounts {
                owed: 0,
                acked: 0,
                resulted: 0,
                degraded: false,
            },
            journal: JournalState {
                last_seq: 0,
                last_presented: 0,
                last_acked: 0,
                unacked_wakes: 0,
            },
            drain_pending: 0,
            afk: 0,
            degraded: vec![],
            joinable: true,
            dispatcher: "up".to_string(),
            held_paths: vec!["src/snapshot.rs".to_string()],
            coalitions: vec![],
            launch_pending: 0,
            loop_state: Default::default(),
            project: "flywheel".to_string(),
            workspace_id: "w1".to_string(),
            progress,
            runs: vec![],
            fleet_workspaces: vec![],
            fleet_agents: vec![],
        };
        let core = status_core_counts(&snap);
        assert_eq!(core["agents"], serde_json::json!(2));
        assert_eq!(core["beads_total"], serde_json::json!(2));
        assert_eq!(core["reservations"], serde_json::json!(1));
        let pv = progress_view(&snap);
        assert_eq!(pv["dispatching_wave"], serde_json::json!(1));
        assert_eq!(pv["project"], serde_json::json!("flywheel"));
        let rv = robot_view(&snap);
        assert_eq!(rv["agents"].as_array().unwrap().len(), 2);
        assert_eq!(rv["core"]["agents"], serde_json::json!(2));
        let sv = serve_view(&snap);
        assert_eq!(sv["core"]["reservations"], serde_json::json!(1));
        assert_eq!(
            crate::snapshot_reporting::progress(&snap)["dispatching_wave"],
            serde_json::json!(1)
        );
        assert_eq!(
            crate::snapshot_reporting::status_core(&snap)["agents"],
            serde_json::json!(2)
        );
    }
}
