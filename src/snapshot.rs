// governed-by: ADR-0010 D10 (bd-67qg)
//! One fleet snapshot contract (`flywheel-snapshot.v1`) + helm drain data.
//!
//! Assembled once per invocation from the workspace model (journal fold,
//! same replay rule as `doorbell why`), beads JSON, herdr agent list, and
//! fallible sources that degrade to empty (never a hard error — a snapshot
//! with degraded sources is still a snapshot; `degraded` names them).
//! status / view / watch / steer_brief render from `assemble`; `helm`
//! drains the three captain sections from it.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Versioned schema id. Robot JSON validates on this + the top-level keys.
pub const SNAPSHOT_SCHEMA: &str = "flywheel-snapshot.v1";

/// One crew pin as the fleet sees it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinRow {
    pub pin: String,
    pub verdict: String,
    pub source: String,
    pub since: String,
    pub bead: Option<String>,
    pub reservations: Vec<String>,
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

/// The v1 snapshot. Every source is fallible; `degraded` names the ones
/// that fell back to empty so operators never read absence as calm.
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
}

/// Top-level keys the robot JSON must carry (schema validation).
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
    ]
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
            crate::doorbell_tick::DoorbellMsg::HerdrStatus { pin, status } => {
                Msg::HerdrStatus { pin, status }
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
    let mut model = crate::workspace::WorkspaceModel::default();
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

/// Assemble the snapshot. `now` is the freshness epoch (caller clock).
pub fn assemble(cfg: &crate::config::Config, project: &str, ws_label: &str, now: u64) -> Snapshot {
    let mut degraded = Vec::new();

    // Pins: herdr agent list for this workspace (best effort).
    let mut pins = Vec::new();
    match crate::herdr_shim::Client::connect(Some(&cfg.socket_path())) {
        Ok(mut c) => match crate::find_workspace(&mut c, project) {
            Ok(ws_id) => match c.agent_list() {
                Ok(agents) => {
                    for a in agents.iter().filter(|a| a.workspace_id == ws_id) {
                        let herdr_status = a.agent_status.as_str();
                        let verdict = crate::workspace::verdict_for(
                            None,
                            herdr_status,
                            herdr_status != "dead",
                        )
                        .status_token();
                        pins.push(PinRow {
                            pin: a.name.clone().unwrap_or_else(|| a.pane_id.clone()),
                            verdict,
                            source: "herdr".to_string(),
                            since: String::new(),
                            bead: None,
                            reservations: Vec::new(),
                        });
                    }
                }
                Err(_) => degraded.push("pins".to_string()),
            },
            Err(_) => degraded.push("pins".to_string()),
        },
        Err(_) => degraded.push("pins".to_string()),
    }

    // Lanes: beads JSON.
    let project_dir: PathBuf = crate::project_dir(cfg, project);
    let (lanes, lanes_degraded) = read_lanes(&project_dir, now);
    if lanes_degraded {
        degraded.push("lanes".to_string());
    }

    // Journal fold: decisions, journal watermarks, drain/afk depths.
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
        .presented
        .values()
        .flatten()
        .map(|e| e.seq)
        .max()
        .unwrap_or(0);
    let last_acked = model.wake_acked.values().copied().max().unwrap_or(0);
    let unacked_wakes = model
        .presented
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
    let drain_pending = model.pending_wait.len();
    let afk = model.digest_queue.len();

    // Mail + reservations: no cheap read path from this process —
    // degraded-empty by contract (fields stay, sources named).
    degraded.push("mail".to_string());
    degraded.push("reservations".to_string());

    Snapshot {
        schema: SNAPSHOT_SCHEMA.to_string(),
        generated_at: now,
        workspace: ws_label.to_string(),
        freshness_epoch: now,
        pins,
        lanes,
        decisions,
        mail: MailCounts {
            owed: 0,
            acked: 0,
            resulted: 0,
            degraded: true,
        },
        journal,
        drain_pending,
        afk,
        degraded,
    }
}

/// Captain bearings: one-line fleet summary for TTY surfaces.
pub fn bearings_line(snap: &Snapshot) -> String {
    format!(
        "fleet: {} pins, {} open decisions, {} afk queued, {} unacked wakes{}",
        snap.pins.len(),
        snap.decisions.len(),
        snap.afk,
        snap.journal.unacked_wakes,
        if snap.degraded.is_empty() {
            String::new()
        } else {
            format!(" (degraded: {})", snap.degraded.join(","))
        }
    )
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
    out.push('\n');
    out
}

/// Wake counters summary for bears (BTreeMap label → count).
pub fn counters_line(counters: &BTreeMap<String, u64>) -> String {
    ["benign", "routine", "captain", "urgent"]
        .iter()
        .map(|k| format!("{k}={}", counters.get(*k).copied().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(" ")
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
                verdict: "Idle(herdr)".into(),
                source: "herdr".into(),
                since: String::new(),
                bead: None,
                reservations: vec![],
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
        // Divergence: no closed beads, open corr c-1 with no linkage rows.
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
}
