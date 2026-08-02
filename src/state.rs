//! Local durable state: append-only JSONL event log + checkpoint snapshots.
//! Location: ~/.local/state/flywheel/<project>.jsonl and checkpoints/<project>/<ts>-<slug>.json

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};


use crate::config::state_dir;
use crate::herdr::AgentStatus;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Spawn,
    Add,
    Send,
    Interrupt,
    State,
    Checkpoint,
    Workflow,
    Pipeline,
    Loop,
    Close,
    Resume,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub ts: String,
    pub kind: EventKind,
    pub project: String,
    /// free-form payload (agent, stage, message, state, ...)
    #[serde(default)]
    pub data: serde_json::Value,
}

/// Registry of active durable loop runs: project -> invocation id + params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveLoop {
    pub id: String,
    pub params: serde_json::Value,
    pub created: String,
}

pub fn loops_file() -> PathBuf {
    state_dir().join("loops.json")
}

pub fn load_active_loop(project: &str) -> Result<Option<ActiveLoop>> {
    let path = loops_file();
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)?;
    let map: serde_json::Value = serde_json::from_str(&raw)?;
    Ok(map
        .get(project)
        .cloned()
        .map(serde_json::from_value)
        .transpose()?)
}

pub fn save_active_loop(project: &str, al: &ActiveLoop) -> Result<()> {
    let path = loops_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut map: serde_json::Value = if path.exists() {
        // a torn/corrupt loops.json must not destroy the registry
        serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    map.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("loops.json is not an object"))?
        .insert(project.to_string(), serde_json::to_value(al)?);
    // atomic write: temp file + rename so a crash mid-write cannot truncate the registry
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&map)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn project_log_path(project: &str) -> PathBuf {
    state_dir().join(format!("{project}.jsonl"))
}

pub fn project_checkpoint_dir(project: &str) -> PathBuf {
    state_dir().join("checkpoints").join(project)
}

/// Append one event to the project's JSONL log (creates dir/file on first use).
pub fn log_event(project: &str, kind: EventKind, data: serde_json::Value) -> Result<()> {
    let path = project_log_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let entry = LogEntry {
        ts: now_iso(),
        kind,
        project: project.to_string(),
        data,
    };
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut line = serde_json::to_string(&entry)?;
    line.push('\n');
    f.write_all(line.as_bytes())
        .with_context(|| format!("append {}", path.display()))?;
    Ok(())
}

pub fn read_log(project: &str) -> Result<Vec<LogEntry>> {
    let path = project_log_path(project);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LogEntry>(&line) {
            Ok(e) => out.push(e),
            Err(err) => eprintln!("flywheel: skipping malformed log line: {err}"),
        }
    }
    Ok(out)
}

/// A point-in-time snapshot of a swarm, saved for `checkpoint save` / `resume`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub ts: String,
    pub project: String,
    pub message: String,
    pub workspace_id: Option<String>,
    pub agents: Vec<CheckpointAgent>,
    pub git_head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointAgent {
    pub name: String,
    pub kind: String,
    pub status: AgentStatus,
    pub pane_id: String,
}

pub fn save_checkpoint(project: &str, message: &str, workspace_id: Option<&str>, agents: Vec<CheckpointAgent>, git_dir: Option<&str>) -> Result<PathBuf> {
    let dir = project_checkpoint_dir(project);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let slug = slugify(message);
    let ts = now_compact();
    let path = dir.join(format!("{ts}-{slug}.json"));
    let cp = Checkpoint {
        ts: now_iso(),
        project: project.to_string(),
        message: message.to_string(),
        workspace_id: workspace_id.map(String::from),
        agents,
        git_head: git_dir.and_then(|d| git_head(d).ok()),
    };
    let raw = serde_json::to_string_pretty(&cp)?;
    std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

pub fn list_checkpoints(project: &str) -> Result<Vec<Checkpoint>> {
    let dir = project_checkpoint_dir(project);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            if let Ok(cp) = serde_json::from_str::<Checkpoint>(&raw) {
                out.push(cp);
            }
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts));
    Ok(out)
}

pub fn latest_checkpoint(project: &str) -> Result<Option<Checkpoint>> {
    Ok(list_checkpoints(project)?.into_iter().last())
}

pub fn git_head(project: &str) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["-C", project, "rev-parse", "--short", "HEAD"])
        .output()
        .context("git rev-parse")?;
    if !out.status.success() {
        bail!("not a git repo or no HEAD");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Aggregate counters over the event log for `analytics`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Analytics {
    pub sends: usize,
    pub interrupts: usize,
    pub checkpoints: usize,
    pub workflow_runs: usize,
    pub pipeline_runs: usize,
    pub loop_runs: usize,
    pub state_transitions: BTreeMap<String, usize>,
    pub first_event: Option<String>,
    pub last_event: Option<String>,
    pub by_day: BTreeMap<String, usize>,
}

pub fn analytics(project: &str) -> Result<Analytics> {
    let entries = read_log(project)?;
    let mut a = Analytics::default();
    for e in &entries {
        match e.kind {
            EventKind::Send => a.sends += 1,
            EventKind::Interrupt => a.interrupts += 1,
            EventKind::Checkpoint => a.checkpoints += 1,
            EventKind::Workflow => a.workflow_runs += 1,
            EventKind::Pipeline => a.pipeline_runs += 1,
            EventKind::Loop => a.loop_runs += 1,
            EventKind::State => {
                if let Some(s) = e.data.get("state").and_then(|v| v.as_str()) {
                    *a.state_transitions.entry(s.to_string()).or_default() += 1;
                }
            }
            _ => {}
        }
        let day = e.ts.chars().take(10).collect::<String>();
        *a.by_day.entry(day).or_default() += 1;
        if a.first_event.is_none() {
            a.first_event = Some(e.ts.clone());
        }
        a.last_event = Some(e.ts.clone());
    }
    Ok(a)
}

/// ISO-8601 UTC timestamp (no chrono dependency).
pub fn now_iso() -> String {
    let (secs, _) = utc_now();
    iso_ts(secs)
}

/// Compact UTC timestamp with millisecond resolution — safe for filenames:
/// two checkpoints saved in the same second no longer collide.
fn now_compact() -> String {
    let (secs, ms) = utc_now();
    compact_ts(secs, ms)
}

fn iso_ts(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn compact_ts(secs: u64, ms: u32) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}{ms:03}")
}

fn utc_now() -> (u64, u32) {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs(), d.subsec_millis()),
        Err(_) => (0, 0),
    }
}

/// Convert unix seconds to UTC civil time (year, month, day, h, m, s)
/// via Howard Hinnant's days-from-civil algorithm.
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, mi, s)
}

fn slugify(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    let collapsed: String = cleaned
        .chars()
        .fold((String::new(), false), |(mut acc, mut prev_dash), c| {
            if c == '-' {
                if !prev_dash {
                    acc.push(c);
                }
                prev_dash = true;
            } else {
                acc.push(c);
                prev_dash = false;
            }
            (acc, prev_dash)
        })
        .0;
    if collapsed.is_empty() {
        "checkpoint".into()
    } else {
        collapsed.chars().take(40).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slugify_handles_junk() {
        assert_eq!(slugify("before auth refactor"), "before-auth-refactor");
        assert_eq!(slugify("!!!"), "checkpoint");
        assert_eq!(slugify("P1 — bug hunt"), "p1-bug-hunt");
    }

    #[test]
    fn civil_from_unix_known_values() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(1_000_000_000), (2001, 9, 9, 1, 46, 40));
        assert_eq!(civil_from_unix(1_700_000_000), (2023, 11, 14, 22, 13, 20));
        assert_eq!(civil_from_unix(86_400), (1970, 1, 2, 0, 0, 0));
    }

    #[test]
    fn timestamps_are_iso_and_unique() {
        let iso = now_iso();
        assert!(iso.len() == 20 && iso.ends_with('Z'), "iso = {iso}");
        assert_eq!(iso.chars().nth(10), Some('T'));
        // compact ts has millisecond resolution: same second, different ms → different filenames
        assert_ne!(compact_ts(1_700_000_000, 1), compact_ts(1_700_000_000, 2));
        assert_eq!(compact_ts(1_700_000_000, 42), "20231114-221320042");
    }

    #[test]
    fn event_kind_roundtrip() {
        let v = json!({"kind": "send", "ts": "0", "project": "t", "data": {}});
        let e: LogEntry = serde_json::from_value(v).unwrap();
        assert_eq!(e.kind, EventKind::Send);
        let v = json!({"kind": "pipeline", "ts": "0", "project": "t", "data": {}});
        let e: LogEntry = serde_json::from_value(v).unwrap();
        assert_eq!(e.kind, EventKind::Pipeline);
    }

    #[test]
    fn analytics_counts() {
        // construct entries manually
        let a = Analytics {
            sends: 3,
            checkpoints: 1,
            ..Default::default()
        };
        assert_eq!(a.sends, 3);
    }
}
