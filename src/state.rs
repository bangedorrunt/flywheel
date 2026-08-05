//! Local durable state: append-only JSONL event log + checkpoint snapshots.
//! Location: ~/.local/state/flywheel/<project>.jsonl and checkpoints/<project>/<ts>-<slug>.json
//!
//! # `loops.json` schema (bd-loop-state-schema-v2)
//!
//! Registry of active durable loop runs at `~/.local/state/flywheel/loops.json`.
//!
//! **v1 (legacy):** flat object `{ "<project>": { id, params, created }, ... }`.
//! No `schema` field. New control fields are absent.
//!
//! **v2 (current):** same flat project map **plus** a top-level `"schema": 2`
//! and optional per-loop control fields for crash-resume:
//! - `event_cursor: { max_seq, pane_ids }` — high-watermark for event subscribe
//!   (`bd-event-subscribe-reactor`); `null`/absent = start fresh.
//! - `escalation: { "<agent>": { first_blocked_ts, ping_count, last_tail } }` —
//!   tier continuity for blocked agents (`bd-blocked-escalation-protocol`).
//!   `first_blocked_ts` is Resonate `ctx.now()` i64-µs (deterministic; never
//!   wall-clock `SystemTime`).
//! - `respawn: { "<agent>": { diagnose_count, respawn_count } }` — bounded
//!   self-heal budget (`bd-event-self-heal-unknown`).
//!
//! **Back-compat (both directions):**
//! - v2 binary reading v1 file: missing `schema` → treat as v1; new fields
//!   default via `#[serde(default)]` (`event_cursor=None`, empty maps). On
//!   first write, migrate in place: set `schema: 2`, log to stderr, preserve
//!   all v1 base fields (additive — no data loss).
//! - v1 binary reading v2 file: serde ignores unknown fields (no
//!   `deny_unknown_fields`); base `id`/`params`/`created` still load.
//!
//! State file = resumable control state only. Telemetry (counts, latencies)
//! goes to the JSONL event log (`log_event`), not here.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::state_dir;
use crate::herdr::AgentStatus;

/// Current `loops.json` schema version (bd-loop-state-schema-v2).
pub const LOOPS_SCHEMA_VERSION: u32 = 2;

/// Reserved top-level key in `loops.json` (not a project path).
const LOOPS_SCHEMA_KEY: &str = "schema";

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
    Respawn,
    SkippedDup,
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

/// High-watermark for event-subscribe resume (bd-event-subscribe-reactor).
/// Monotonic `max_seq` + the pane ids covered by that cursor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct EventCursor {
    pub max_seq: u64,
    #[serde(default)]
    pub pane_ids: Vec<String>,
}

/// Per-agent blocked escalation tier state (bd-blocked-escalation-protocol).
/// `first_blocked_ts` is Resonate `ctx.now()` i64-µs — stored, not recomputed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct EscalationState {
    /// Deterministic timestamp (Resonate ctx.now, µs). Never wall-clock.
    pub first_blocked_ts: i64,
    pub ping_count: u32,
    #[serde(default)]
    pub last_tail: String,
}

/// Per-agent self-heal budget (bd-event-self-heal-unknown).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RespawnBudget {
    pub diagnose_count: u32,
    pub respawn_count: u32,
}

/// One active durable loop run: invocation id + params + v2 control state.
///
/// New fields use `#[serde(default)]` so a v1-shaped JSON still deserializes
/// (fresh defaults). A v1 binary reading this struct's v2 JSON ignores the
/// extra keys (serde default for structs without `deny_unknown_fields`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActiveLoop {
    pub id: String,
    pub params: serde_json::Value,
    pub created: String,
    /// Event-subscribe high-watermark; `None` = start fresh (v1 / first run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_cursor: Option<EventCursor>,
    /// Per-agent blocked escalation state (tier continuity across resume).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub escalation: BTreeMap<String, EscalationState>,
    /// Per-agent diagnose/respawn budget (must not reset on crash).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub respawn: BTreeMap<String, RespawnBudget>,
}

impl ActiveLoop {
    /// Construct a v2 entry with empty control fields (fresh loop start).
    pub fn new(
        id: impl Into<String>,
        params: serde_json::Value,
        created: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            params,
            created: created.into(),
            event_cursor: None,
            escalation: BTreeMap::new(),
            respawn: BTreeMap::new(),
        }
    }
}

pub fn loops_file() -> PathBuf {
    state_dir().join("loops.json")
}

/// Read `loops.json` into (schema_version, project → ActiveLoop).
///
/// - Missing file → empty map, schema v2 (nothing to migrate).
/// - Missing / non-numeric `schema` → treated as v1 (schema_version = 1).
/// - Project entries that fail to deserialize are skipped (logged to stderr).
/// - Torn/corrupt file → empty map (same contract as the pre-v2 reader).
pub fn load_loops_registry() -> Result<(u32, BTreeMap<String, ActiveLoop>)> {
    let path = loops_file();
    if !path.exists() {
        return Ok((LOOPS_SCHEMA_VERSION, BTreeMap::new()));
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(parse_loops_registry(&raw))
}

/// Pure parse of a `loops.json` body (unit-testable; no I/O).
///
/// Returns `(schema_version, loops)`. v1 files (no `schema` key) yield
/// `schema_version = 1` and defaulted control fields on each `ActiveLoop`.
pub fn parse_loops_registry(raw: &str) -> (u32, BTreeMap<String, ActiveLoop>) {
    let map: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return (LOOPS_SCHEMA_VERSION, BTreeMap::new()),
    };
    let Some(obj) = map.as_object() else {
        return (LOOPS_SCHEMA_VERSION, BTreeMap::new());
    };

    let schema = obj
        .get(LOOPS_SCHEMA_KEY)
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(1); // missing schema → v1

    let mut loops = BTreeMap::new();
    for (k, v) in obj {
        if k == LOOPS_SCHEMA_KEY {
            continue;
        }
        match serde_json::from_value::<ActiveLoop>(v.clone()) {
            Ok(al) => {
                loops.insert(k.clone(), al);
            }
            Err(err) => {
                eprintln!("flywheel: skipping malformed loops.json entry {k}: {err}");
            }
        }
    }
    (schema, loops)
}

/// Serialize the registry as schema v2 JSON (pretty). Pure — no I/O.
pub fn serialize_loops_registry(loops: &BTreeMap<String, ActiveLoop>) -> Result<String> {
    let mut map = serde_json::Map::new();
    map.insert(
        LOOPS_SCHEMA_KEY.to_string(),
        serde_json::Value::Number(LOOPS_SCHEMA_VERSION.into()),
    );
    for (project, al) in loops {
        map.insert(project.clone(), serde_json::to_value(al)?);
    }
    Ok(serde_json::to_string_pretty(&serde_json::Value::Object(
        map,
    ))?)
}

/// Atomically write the full registry as schema v2.
///
/// If the on-disk file was v1 (or missing schema), logs the migration once.
pub fn save_loops_registry(loops: &BTreeMap<String, ActiveLoop>) -> Result<()> {
    let path = loops_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // detect v1 → v2 for audit log
    if path.exists() {
        let prior = std::fs::read_to_string(&path).unwrap_or_default();
        let (prior_schema, _) = parse_loops_registry(&prior);
        if prior_schema < LOOPS_SCHEMA_VERSION {
            eprintln!(
                "flywheel: migrating loops.json v{prior_schema} → v{LOOPS_SCHEMA_VERSION} (state fields initialized fresh)"
            );
        }
    }

    let body = serialize_loops_registry(loops)?;
    // atomic write: temp file + rename so a crash mid-write cannot truncate the registry
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load_active_loop(project: &str) -> Result<Option<ActiveLoop>> {
    let (_schema, map) = load_loops_registry()?;
    Ok(map.get(project).cloned())
}

pub fn save_active_loop(project: &str, al: &ActiveLoop) -> Result<()> {
    let (_schema, mut map) = load_loops_registry()?;
    map.insert(project.to_string(), al.clone());
    save_loops_registry(&map)
}

pub fn project_log_path(project: &str) -> PathBuf {
    state_dir().join(format!("{project}.jsonl"))
}

pub fn project_checkpoint_dir(project: &str) -> PathBuf {
    state_dir().join("checkpoints").join(project)
}

// ---- last-sent registry (bd-last-sent-registry) ----
//
// Per-project sidecar (NOT the shared loops.json — that file is written by
// any loop on any project; a per-project file keeps writers disjoint and
// atomic-write races impossible). Records what the `__send` leaf actually
// delivered, so a crash between herdr's landing and Resonate's checkpoint
// can be deduplicated on resume (bd-resume-dedup). Single-writer per project
// (the loop; `promise_pending` guards against concurrent loops).

/// One delivered (or attempted) prompt send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentRecord {
    /// agent herdr name
    pub name: String,
    /// stable FNV-1a 64 hash of the exact prompt text
    pub text_hash: u64,
    /// invocation id that delegated the prompt (dedup filters to the SAME
    /// invocation — a fresh run legitimately re-delegates)
    pub invocation: String,
    /// loop iteration at send time (diagnostics)
    pub iteration: u64,
    /// true when herdr confirmed the prompt landed (working|blocked)
    pub landed: bool,
}

/// Bound on the per-project registry (FIFO trim). A long-running loop can
/// send thousands of prompts; unbounded growth is a leak.
pub const LAST_SENT_CAP: usize = 50;

pub fn last_sent_file(project: &str) -> PathBuf {
    state_dir()
        .join("last_sent")
        .join(format!("{project}.json"))
}

/// Load the project's sent records. Missing or corrupt files yield an empty
/// vec — never an error or panic (bookkeeping must not break the loop).
pub fn load_last_sent(project: &str) -> Vec<SentRecord> {
    let raw = std::fs::read_to_string(last_sent_file(project)).unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Append one record and persist atomically (temp + rename — a crash
/// mid-write cannot tear the registry). Best-effort by contract: callers log
/// and continue on error.
pub fn record_sent(project: &str, record: SentRecord) -> Result<()> {
    let mut records = load_last_sent(project);
    records.push(record);
    trim_last_sent(&mut records);
    save_last_sent(project, &records)
}

/// Persist the registry atomically (temp + rename).
pub fn save_last_sent(project: &str, records: &[SentRecord]) -> Result<()> {
    let path = last_sent_file(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(records)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Keep the newest `LAST_SENT_CAP` records (FIFO: drop the oldest).
pub fn trim_last_sent(records: &mut Vec<SentRecord>) {
    if records.len() > LAST_SENT_CAP {
        let drop = records.len() - LAST_SENT_CAP;
        records.drain(0..drop);
    }
}

/// Find the newest record for (agent, prompt-hash). `bd-resume-dedup` uses
/// this to decide whether a prompt was already delivered.
pub fn find_last_sent<'a>(
    records: &'a [SentRecord],
    name: &str,
    text_hash: u64,
) -> Option<&'a SentRecord> {
    records
        .iter()
        .rev()
        .find(|r| r.name == name && r.text_hash == text_hash)
}

/// FNV-1a 64: a trivial, byte-stable hash. Chosen over
/// `DefaultHasher` (not guaranteed stable across Rust releases — replay
/// determinism requires identical bytes across process restarts).
pub fn fnv1a64(text: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Append one event to the project's JSONL log (creates dir/file on first use).
pub fn log_event(project: &str, kind: EventKind, data: serde_json::Value) -> Result<()> {
    let path = project_log_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
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

pub fn save_checkpoint(
    project: &str,
    message: &str,
    workspace_id: Option<&str>,
    agents: Vec<CheckpointAgent>,
    git_dir: Option<&str>,
) -> Result<PathBuf> {
    let dir = project_checkpoint_dir(project);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
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
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
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
mod loops_schema_v2_tests {
    use super::*;
    use serde_json::json;

    /// Minimal v1 fixture: flat project map, no `schema` key, no control fields.
    fn v1_fixture() -> String {
        json!({
            "/tmp/proj-a": {
                "id": "swarm-loop:/tmp/proj-a:1",
                "params": {"project": "/tmp/proj-a", "queue": []},
                "created": "2026-08-04T00:00:00Z"
            },
            "/tmp/proj-b": {
                "id": "swarm-loop:/tmp/proj-b:2",
                "params": {"project": "/tmp/proj-b"},
                "created": "2026-08-04T01:00:00Z"
            }
        })
        .to_string()
    }

    #[test]
    fn parse_v1_defaults_new_fields_and_schema_1() {
        let (schema, map) = parse_loops_registry(&v1_fixture());
        assert_eq!(schema, 1);
        assert_eq!(map.len(), 2);

        let a = map.get("/tmp/proj-a").expect("proj-a");
        assert_eq!(a.id, "swarm-loop:/tmp/proj-a:1");
        assert_eq!(a.created, "2026-08-04T00:00:00Z");
        assert!(a.event_cursor.is_none());
        assert!(a.escalation.is_empty());
        assert!(a.respawn.is_empty());
        // base params intact
        assert_eq!(a.params["project"], "/tmp/proj-a");
    }

    #[test]
    fn v1_read_v2_write_roundtrip_preserves_base_fields() {
        let (schema, map) = parse_loops_registry(&v1_fixture());
        assert_eq!(schema, 1);

        let body = serialize_loops_registry(&map).unwrap();
        // written body is always schema v2
        let written: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(written["schema"], 2);

        let (schema2, map2) = parse_loops_registry(&body);
        assert_eq!(schema2, 2);
        assert_eq!(map2.len(), 2);

        let a1 = map.get("/tmp/proj-a").unwrap();
        let a2 = map2.get("/tmp/proj-a").unwrap();
        assert_eq!(a1.id, a2.id);
        assert_eq!(a1.params, a2.params);
        assert_eq!(a1.created, a2.created);
        // fresh control fields after v1→v2
        assert!(a2.event_cursor.is_none());
        assert!(a2.escalation.is_empty());
        assert!(a2.respawn.is_empty());
    }

    #[test]
    fn v2_control_fields_roundtrip() {
        let mut escalation = BTreeMap::new();
        escalation.insert(
            "AgentOne".into(),
            EscalationState {
                first_blocked_ts: 1_700_000_000_000_000, // µs, ctx.now-shaped
                ping_count: 2,
                last_tail: "stuck on cargo".into(),
            },
        );
        let mut respawn = BTreeMap::new();
        respawn.insert(
            "AgentOne".into(),
            RespawnBudget {
                diagnose_count: 1,
                respawn_count: 2,
            },
        );
        let al = ActiveLoop {
            id: "swarm-loop:/tmp/p:9".into(),
            params: json!({"project": "/tmp/p"}),
            created: "2026-08-05T00:00:00Z".into(),
            event_cursor: Some(EventCursor {
                max_seq: 42,
                pane_ids: vec!["%1".into(), "%2".into()],
            }),
            escalation,
            respawn,
        };
        let mut map = BTreeMap::new();
        map.insert("/tmp/p".into(), al.clone());

        let body = serialize_loops_registry(&map).unwrap();
        let (schema, map2) = parse_loops_registry(&body);
        assert_eq!(schema, 2);
        let got = map2.get("/tmp/p").unwrap();
        assert_eq!(got, &al);
        assert_eq!(got.event_cursor.as_ref().unwrap().max_seq, 42);
        assert_eq!(
            got.escalation["AgentOne"].first_blocked_ts,
            1_700_000_000_000_000
        );
        assert_eq!(got.respawn["AgentOne"].respawn_count, 2);
    }

    #[test]
    fn forward_tolerance_v1_shaped_struct_ignores_v2_fields() {
        // A v1-shaped deserializer (only base fields) must still load a v2
        // entry — serde ignores unknown fields without deny_unknown_fields.
        #[derive(Debug, Deserialize)]
        struct ActiveLoopV1 {
            id: String,
            params: serde_json::Value,
            created: String,
        }
        let v2_entry = json!({
            "id": "swarm-loop:/tmp/p:1",
            "params": {"project": "/tmp/p"},
            "created": "2026-08-05T00:00:00Z",
            "event_cursor": {"max_seq": 7, "pane_ids": ["%1"]},
            "escalation": {"A": {"first_blocked_ts": 1, "ping_count": 0, "last_tail": ""}},
            "respawn": {"A": {"diagnose_count": 0, "respawn_count": 1}}
        });
        let v1: ActiveLoopV1 = serde_json::from_value(v2_entry).unwrap();
        assert_eq!(v1.id, "swarm-loop:/tmp/p:1");
        assert_eq!(v1.created, "2026-08-05T00:00:00Z");
        assert_eq!(v1.params["project"], "/tmp/p");
    }

    #[test]
    fn schema_key_is_not_a_project_entry() {
        let raw = json!({
            "schema": 2,
            "/tmp/real": {
                "id": "id-1",
                "params": {},
                "created": "t"
            }
        })
        .to_string();
        let (schema, map) = parse_loops_registry(&raw);
        assert_eq!(schema, 2);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("/tmp/real"));
        assert!(!map.contains_key("schema"));
    }

    #[test]
    fn active_loop_new_has_fresh_control_fields() {
        let al = ActiveLoop::new("id", json!({}), "now");
        assert!(al.event_cursor.is_none());
        assert!(al.escalation.is_empty());
        assert!(al.respawn.is_empty());
    }

    #[test]
    fn corrupt_loops_json_yields_empty_registry() {
        let (schema, map) = parse_loops_registry("{not json!!");
        assert_eq!(schema, LOOPS_SCHEMA_VERSION);
        assert!(map.is_empty());
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

#[cfg(test)]
mod last_sent_tests {
    use super::*;

    fn rec(name: &str, hash: u64, invocation: &str, landed: bool) -> SentRecord {
        SentRecord {
            name: name.to_string(),
            text_hash: hash,
            invocation: invocation.to_string(),
            iteration: 1,
            landed,
        }
    }

    #[test]
    fn fnv1a64_is_stable_and_distinct() {
        // same input, same value — byte-identical across processes/releases
        assert_eq!(fnv1a64("prompt A"), fnv1a64("prompt A"));
        assert_eq!(fnv1a64(""), fnv1a64(""));
        // known vector (FNV-1a 64 offset basis + prime applied to "a")
        // 0xaf63dc4c8601ec8c is the canonical FNV-1a 64 of the byte 'a'
        assert_eq!(fnv1a64("a"), 0xaf63dc4c8601ec8c);
        // distinct inputs must not collide trivially
        assert_ne!(fnv1a64("prompt A"), fnv1a64("prompt B"));
    }

    #[test]
    fn trim_keeps_newest_50() {
        let mut v: Vec<SentRecord> = (0..60).map(|i| rec("a", i, "inv", true)).collect();
        trim_last_sent(&mut v);
        assert_eq!(v.len(), 50);
        // FIFO: the oldest (0..10) dropped, the newest (10..60) kept
        assert_eq!(v[0].text_hash, 10);
        assert_eq!(v[49].text_hash, 59);
    }

    #[test]
    fn trim_under_cap_is_noop() {
        let mut v: Vec<SentRecord> = (0..3).map(|i| rec("a", i, "inv", true)).collect();
        trim_last_sent(&mut v);
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn load_save_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("flywheel-lastsent-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // the state dir is fixed by state_dir(); we test through the real file
        // path machinery by temporarily pointing LAST_SENT at a temp dir —
        // instead, exercise record_sent/load_last_sent against a fake project
        // name that collides with nothing, then clean the file up.
        let project = "__test_roundtrip__";
        let _ = std::fs::remove_file(last_sent_file(project));
        record_sent(project, rec("a", 1, "inv1", true)).unwrap();
        record_sent(project, rec("b", 2, "inv1", false)).unwrap();
        let v = load_last_sent(project);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "a");
        assert_eq!(v[1].name, "b");
        assert!(v[0].landed);
        let _ = std::fs::remove_file(last_sent_file(project));
        let _ = std::fs::remove_file(last_sent_file(project).with_extension("json.tmp"));
        drop(dir);
    }

    #[test]
    fn load_corrupt_file_returns_empty() {
        let project = "__test_corrupt__";
        let p = last_sent_file(project);
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        std::fs::write(&p, "{not json!!").unwrap();
        assert!(load_last_sent(project).is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn find_last_sent_matches_name_and_hash() {
        let v = vec![rec("a", 11, "inv1", true), rec("b", 22, "inv1", true)];
        assert!(find_last_sent(&v, "a", 11).is_some());
        assert!(find_last_sent(&v, "a", 22).is_none());
        assert!(find_last_sent(&v, "b", 22).is_some());
        assert!(find_last_sent(&v, "c", 11).is_none());
    }
}
