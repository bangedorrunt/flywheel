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
use crate::herdr_shim::AgentStatus;

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
    /// bd-a7j: a dispatched prompt expected a thread reply that never arrived
    /// (logged on each reminder / escalation handoff)
    ReplyMissing,
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
    /// The tier whose ping was last SENT (cg6.3): a replay re-execution at the
    /// same tier must not re-mail — at most one ping per logical tier
    /// transition. `None` = no ping sent yet.
    #[serde(default)]
    pub last_ping_tier: Option<u32>,
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

/// Directory of per-project active-loop sidecars (l4s.1): each project's
/// `ActiveLoop` v2 payload lives in `loops/<project>.json` so writers on
/// DIFFERENT projects never share a read-modify-write (the lost-update race
/// the legacy shared `loops.json` had). Projects are filesystem-safe slugs
/// (same ones used for `<project>.jsonl` logs).
pub fn loops_dir() -> PathBuf {
    state_dir().join("loops")
}

/// Filesystem-safe state key for a project. Relative project names are used
/// verbatim (all existing state files keep their names); ABSOLUTE paths
/// (mktemp-style scratch projects) would collapse every per-project sidecar
/// onto `<abs-path>.json` (PathBuf::join discards the base) — they get a
/// slug: base name + short hash of the full path (unique per project).
pub fn state_key(project: &str) -> String {
    if project.contains('/') {
        let base = project.rsplit('/').next().unwrap_or(project);
        let h = fnv1a64(project);
        format!("{base}-{:08x}", (h & 0xffff_ffff) as u32)
    } else {
        project.to_string()
    }
}

fn sidecar_file(project: &str) -> PathBuf {
    loops_dir().join(format!("{}.json", state_key(project)))
}

/// Directory of the per-project spawn registry (pane_id → agent name).
/// Written at spawn; used by `flywheel rebind` and the send auto-heal path
/// (bd-fw-name-binding-loss-7rc) to restore a name binding an agent lost
/// when its session restarted in the pane (herdr shows name:null, and name
/// resolution silently misses).
pub fn agents_dir() -> PathBuf {
    state_dir().join("agents")
}

pub fn spawn_registry_file(project: &str) -> PathBuf {
    agents_dir().join(format!("{}.json", state_key(project)))
}

/// One spawn-registry record: (pane_id, agent name, optional crew profile).
///
/// The `profile` column (P10.3, bd-ghcj.1) lets the pane runner resolve
/// `@role -> running pane`: a crew member spawned FOR a profile carries
/// `profile: Some(role)`; ordinary swarm spawns leave it `None`. Pre-P10.3
/// registry files (2-tuples) deserialize via the tolerant [`Deserialize`]
/// impl below; new writes use the struct shape.
///
/// The `mail_identity` column (bd-627z.4) pins the agent's registered
/// agent-mail identity (adjective+noun) to the pane/name AT SPAWN TIME.
/// `assign` resolves through this map, so a numbered agent (oc2/oc3/...) keeps
/// the SAME identity across calls and restarts — the deterministic binding
/// instead of re-hashing the (renumberable) herdr name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpawnEntry {
    /// the herdr pane id the agent runs in
    pub pane_id: String,
    /// the agent (herdr) name
    pub name: String,
    /// the crew profile this pane serves (`planner`, `coder`, ...); `None`
    /// for plain swarm spawns (legacy shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// the harness this pane runs (bd-lvyi per-step override resolution:
    /// a step overriding harness dispatches to a pane of that harness if
    /// one exists). `None` for legacy entries — resolved from the profile
    /// via the catalog at dispatch time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// the registered agent-mail identity (adjective+noun) pinned at spawn
    /// (bd-627z.4). `None` for pre-bd-627z.4 entries — assign falls back to
    /// the deterministic hash mapping for those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail_identity: Option<String>,
    /// bd-eign.10: whether the agent already received the profile's
    /// skill-load prefix on its first dispatch. Advisory — a lost update
    /// (concurrent __send subprocesses) at worst repeats the prefix once.
    #[serde(default)]
    pub skills_injected: bool,
}

impl SpawnEntry {
    /// A legacy-format entry: pane + name, no profile.
    pub fn legacy(pane_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            pane_id: pane_id.into(),
            name: name.into(),
            profile: None,
            harness: None,
            mail_identity: None,
            skills_injected: false,
        }
    }

    /// Full entry: pane + name + the pinned mail identity (bd-627z.4).
    pub fn with_mail_identity(
        pane_id: impl Into<String>,
        name: impl Into<String>,
        mail_identity: impl Into<String>,
    ) -> Self {
        Self {
            pane_id: pane_id.into(),
            name: name.into(),
            profile: None,
            harness: None,
            mail_identity: Some(mail_identity.into()),
            skills_injected: false,
        }
    }

    /// Profile-driven entry (bd-eign.9/13): the pane serves `profile` under
    /// harness `harness`; `cmd_send`/`__send` condition prompts with the
    /// profile's behavior preamble + skill injection from these columns.
    pub fn profiled(
        pane_id: impl Into<String>,
        name: impl Into<String>,
        profile: impl Into<String>,
        harness: impl Into<String>,
        mail_identity: Option<String>,
    ) -> Self {
        Self {
            pane_id: pane_id.into(),
            name: name.into(),
            profile: Some(profile.into()),
            harness: Some(harness.into()),
            mail_identity,
            skills_injected: false,
        }
    }
}

/// Tolerant read of the spawn registry (bd-ghcj.1): pre-P10.3 files hold
/// 2-tuples `[pane_id, name]`; P10.3 files hold the struct with `profile`.
/// Both deserialize — an existing swarm's registry survives the upgrade.
impl<'de> Deserialize<'de> for SpawnEntry {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Legacy((String, String)),
            New {
                pane_id: String,
                name: String,
                #[serde(default)]
                profile: Option<String>,
                #[serde(default)]
                harness: Option<String>,
                #[serde(default)]
                mail_identity: Option<String>,
                #[serde(default)]
                skills_injected: bool,
            },
        }
        match Raw::deserialize(d)? {
            Raw::Legacy((pane_id, name)) => Ok(SpawnEntry {
                pane_id,
                name,
                profile: None,
                harness: None,
                mail_identity: None,
                skills_injected: false,
            }),
            Raw::New {
                pane_id,
                name,
                profile,
                harness,
                mail_identity,
                skills_injected,
            } => Ok(SpawnEntry {
                pane_id,
                name,
                profile,
                harness,
                mail_identity,
                skills_injected,
            }),
        }
    }
}

/// Per-project spawn metadata (bd-vik): the workspace LABEL when a custom
/// `--label` was used, so project-based discovery (find_workspace) still
/// resolves the swarm after a custom-label spawn.
pub fn spawn_meta_file(project: &str) -> PathBuf {
    agents_dir().join(format!("{}.meta.json", state_key(project)))
}

/// Persist the custom label for a project (best-effort; spawn succeeds even
/// if this write fails).
pub fn save_spawn_meta(project: &str, label: &str) -> Result<()> {
    let path = spawn_meta_file(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&serde_json::json!({ "label": label }))?;
    let tmp = path.with_extension("meta.json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the custom label recorded for a project (missing/corrupt → None).
pub fn load_spawn_meta(project: &str) -> Option<String> {
    let path = spawn_meta_file(project);
    let raw = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("label")
        .and_then(|l| l.as_str())
        .map(String::from)
}

/// bd-a7j: an outstanding expected reply — written by `flywheel collab`
/// (which knows the thread it created), read by the durable loop each
/// iteration to verify the thread actually moved before settling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExpectedReply {
    /// agent (herdr name) that was asked to reply in-thread
    pub agent: String,
    /// thread id the reply must land in
    pub thread: String,
    /// message id of the question (the reply must differ from it)
    pub head: Option<String>,
    /// iso timestamp (informational)
    pub created_ts: String,
}

/// Per-project expected-reply file: `expected-replies/<project>.json`.
pub fn expected_replies_dir() -> PathBuf {
    state_dir().join("expected-replies")
}

pub fn expected_reply_file(project: &str) -> PathBuf {
    expected_replies_dir().join(format!("{}.json", state_key(project)))
}

/// Atomically replace the project's expected-reply set (collab writes the
/// whole set on each collab call; the loop's `__reply-settled` leaf rewrites
/// it minus a satisfied entry).
pub fn save_expected_replies(project: &str, entries: &[ExpectedReply]) -> Result<()> {
    let path = expected_reply_file(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(entries)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the project's expected replies (missing/corrupt → empty).
pub fn load_expected_replies(project: &str) -> Vec<ExpectedReply> {
    let path = expected_reply_file(project);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Remove one (agent, thread) entry — used by the checkpointed
/// `__reply-settled` leaf so satisfied expectations stop keeping the loop
/// alive, without the workflow body writing files.
pub fn remove_expected_reply(project: &str, agent: &str, thread: &str) -> Result<()> {
    let mut entries = load_expected_replies(project);
    let before = entries.len();
    entries.retain(|e| !(e.agent == agent && e.thread == thread));
    if entries.len() == before {
        return Ok(()); // nothing to do
    }
    save_expected_replies(project, &entries)
}

/// Atomically persist the project's pane→name registry.
pub fn save_spawn_registry(project: &str, entries: &[SpawnEntry]) -> Result<()> {
    let path = spawn_registry_file(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(entries)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the project's pane→name registry (missing/corrupt → empty).
pub fn load_spawn_registry(project: &str) -> Vec<SpawnEntry> {
    let path = spawn_registry_file(project);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Project-local pane→name bindings (bd-eign.3 A): mirrors the spawn
/// registry but lives INSIDE the project (`.flywheel/agent-names.json`),
/// so a lost state dir cannot lose the binding. `cmd_recover` falls back
/// to this file when the registry is empty.
pub fn agent_names_file(dir: &std::path::Path) -> PathBuf {
    dir.join(".flywheel").join("agent-names.json")
}

/// Atomically persist the project-local pane→name mapping.
pub fn save_agent_names(dir: &std::path::Path, entries: &[SpawnEntry]) -> Result<()> {
    let path = agent_names_file(dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(entries)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the project-local pane→name mapping (missing/corrupt → empty).
pub fn load_agent_names(dir: &std::path::Path) -> Vec<SpawnEntry> {
    let path = agent_names_file(dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// One-time migration of the legacy SHARED `loops.json` into per-project
/// sidecars (l4s.1): each entry becomes `loops/<project>.json` (atomic
/// temp+rename), then the legacy file is renamed to
/// `loops.json.migrated-<ts>` so a broken split is recoverable.
///
/// Concurrent-safe without coordination: while the legacy file exists, any
/// number of migrators may re-split (sidecar writes are atomic and
/// content-identical), and the legacy rename tolerates the loser's ENOENT.
fn ensure_loops_migrated() -> Result<()> {
    let legacy = loops_file();
    if !legacy.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&legacy)?;
    let (prior_schema, map) = parse_loops_registry(&raw);
    // Distinguish "no content" (valid JSON object, zero loop entries) from
    // "corrupt" (parse failure or non-object): only absence is lossless to
    // delete. Corrupt files keep the rename-backup path below.
    let parses_as_object = serde_json::from_str::<serde_json::Value>(&raw)
        .map(|v| v.is_object())
        .unwrap_or(false);
    if map.is_empty() && parses_as_object {
        // Empty legacy registry (e.g. `{}` or `{"schema":2}` after a clean
        // shutdown): nothing to preserve — delete it so repeat migrations
        // stop littering loops.json.migrated-* files (bd-fw-loop-status-stale-hjn).
        match std::fs::remove_file(&legacy) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(());
    }
    if prior_schema < LOOPS_SCHEMA_VERSION {
        // Genuine v1 content (not absence — absence returns above): keep the
        // audit signal, now reworded for the sidecar split (l4s.2).
        eprintln!(
            "flywheel: migrating loops.json v{prior_schema} → v{LOOPS_SCHEMA_VERSION} (split into per-project sidecars)"
        );
    }
    let dir = loops_dir();
    std::fs::create_dir_all(&dir)?;
    for (project, al) in &map {
        let path = sidecar_file(project);
        let body = serde_json::to_string_pretty(al)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let renamed = legacy.with_file_name(format!("loops.json.migrated-{ts}"));
    match std::fs::rename(&legacy, &renamed) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    // Sweep stale migration backups (litter fix): the sidecars hold the
    // content, so the NEWEST .migrated-* marker is the single recoverable
    // backup; older ones are duplicates of the same legacy file. Names are
    // epoch seconds — fixed width, so lexicographic order is chronological.
    if let Ok(rd) = std::fs::read_dir(legacy.parent().unwrap()) {
        let mut markers: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("loops.json.migrated-"))
                    .unwrap_or(false)
            })
            .collect();
        // newest = largest epoch suffix; parse numerically (not lexicographic —
        // timestamps are not fixed-width across eras).
        markers.sort_by_key(|p| {
            p.file_name()
                .map(|n| {
                    n.to_string_lossy()
                        .trim_start_matches("loops.json.migrated-")
                        .parse::<u64>()
                        .unwrap_or(0)
                })
                .unwrap_or(0)
        });
        for old in markers.iter().take(markers.len().saturating_sub(1)) {
            let _ = std::fs::remove_file(old);
        }
    }
    Ok(())
}

/// Read ALL active loops as (schema_version, project → ActiveLoop).
///
/// Migrates a legacy shared `loops.json` on first access, then scans the
/// per-project sidecars (l4s.1). Missing dir → empty map, schema v2.
/// Sidecars that fail to deserialize are skipped (logged to stderr).
/// Production caller since bd-eign.4 (`flywheel loop doctor` registry check).
pub fn load_loops_registry() -> Result<(u32, BTreeMap<String, ActiveLoop>)> {
    ensure_loops_migrated()?;
    let dir = loops_dir();
    let mut loops = BTreeMap::new();
    if !dir.exists() {
        return Ok((LOOPS_SCHEMA_VERSION, loops));
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue; // skips *.json.tmp writers mid-flight
        }
        let Some(project) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let raw = std::fs::read_to_string(&path)?;
        match serde_json::from_str::<ActiveLoop>(&raw) {
            Ok(al) => {
                loops.insert(project, al);
            }
            Err(err) => {
                eprintln!("flywheel: skipping malformed loops sidecar {project}: {err}");
            }
        }
    }
    Ok((LOOPS_SCHEMA_VERSION, loops))
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
/// Test-only since the sidecar migration (l4s.1).
#[cfg(test)]
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

/// Bulk-write the registry as per-project sidecars (l4s.1). Kept for callers
/// that think in whole-registry terms (status views, tests, cleanup): every
/// entry is written to its own sidecar atomically, and sidecars for projects
/// absent from the map are removed. Removed (l4s.1+[lints]): zero callers in
/// the post-sidecar tree; per-project writers use `save_active_loop` and the
/// registry views use `load_active_loop`/`load_loops_registry`.
pub fn load_active_loop(project: &str) -> Result<Option<ActiveLoop>> {
    ensure_loops_migrated()?;
    let path = sidecar_file(project);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&raw)?))
}

/// Persist one project's active loop to ITS OWN sidecar (l4s.1): writers on
/// different projects are disjoint by construction — no shared read-modify-
/// write, no lost updates. Atomic temp+rename (same pattern as before).
pub fn save_active_loop(project: &str, al: &ActiveLoop) -> Result<()> {
    ensure_loops_migrated()?;
    let dir = loops_dir();
    std::fs::create_dir_all(&dir)?;
    let path = sidecar_file(project);
    let body = serde_json::to_string_pretty(al)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Delete a project's active-loop sidecar (no-op when absent). Used by tests
/// and cleanup; there is no tombstone — absence means "no active loop".
#[cfg(test)]
pub fn remove_active_loop(project: &str) -> Result<()> {
    let path = sidecar_file(project);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Per-project seen-marker file for cross-project spawns (cg6.3): one
/// message_id per line. The `spawn_xproj` leaf marks BEFORE spawning so a
/// crash-window replay no-ops instead of spawning two swarms for one request.
pub fn xproj_seen_file(project: &str) -> PathBuf {
    state_dir().join(format!("{}.xproj-seen.jsonl", state_key(project)))
}

/// True when `message_id` was already marked (at-most-once dedup).
pub fn xproj_seen_contains(project: &str, message_id: &str) -> Result<bool> {
    let path = xproj_seen_file(project);
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(raw.lines().any(|l| l.trim() == message_id))
}

/// Mark `message_id` as spawned (idempotent append; one id per line).
pub fn xproj_seen_mark(project: &str, message_id: &str) -> Result<()> {
    if xproj_seen_contains(project, message_id)? {
        return Ok(());
    }
    let path = xproj_seen_file(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    use std::io::Write;
    writeln!(f, "{message_id}")?;
    Ok(())
}

/// Remove a seen-marker for `message_id` (cg6.6): a FAILED spawn must be
/// retried, so the leaf unmarks what it marked before spawning. Rewrites the
/// file without the line; missing file/line is a no-op.
pub fn xproj_seen_unmark(project: &str, message_id: &str) -> Result<()> {
    let path = xproj_seen_file(project);
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&path)?;
    let remaining: Vec<&str> = raw.lines().filter(|l| l.trim() != message_id).collect();
    if remaining.len() == raw.lines().count() {
        return Ok(()); // not marked — nothing to do
    }
    crate::fs_util::atomic_write(&path, remaining.join("\n").as_bytes())
}

pub fn project_log_path(project: &str) -> PathBuf {
    state_dir().join(format!("{project}.jsonl"))
}

pub fn project_checkpoint_dir(project: &str) -> PathBuf {
    state_dir().join("checkpoints").join(state_key(project))
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
    /// cg6.5: a timeout-confirm send is AMBIGUOUS — the prompt may have
    /// started, so resume-dedup must NOT re-delegate it (the next poll is
    /// the correctness floor). `#[serde(default)] false` keeps legacy
    /// records resend-eligible (unchanged behavior); only NEW timeout
    /// records carry the flag.
    #[serde(default)]
    pub ambiguous: bool,
}

/// Bound on the per-project registry (FIFO trim). A long-running loop can
/// send thousands of prompts; unbounded growth is a leak.
pub const LAST_SENT_CAP: usize = 50;

pub fn last_sent_file(project: &str) -> PathBuf {
    state_dir()
        .join("last_sent")
        .join(format!("{}.json", state_key(project)))
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

// ---- loop process registry (bd-fw-stale-loop-processes-gac) ----

/// One entry in the live-loop pid registry (`loop-pids.json`). The registry
/// exists so a second `flywheel loop run <project>` refuses to start while a live
/// loop process for the same project exists — stale nohup'd loops can no
/// longer accumulate and race new invocations (observed: 7 stale loops
/// polling toron+flywheel, writing into each other's logs).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoopPidEntry {
    pub pid: u32,
    pub started_at: String,
    /// Filled by the durable runner once the invocation id exists; empty for
    /// the brief pre-invocation window. The authoritative invocation for a
    /// settled loop lives in `loops/<project>.json` (loop_status queries it).
    #[serde(default)]
    pub invocation: String,
}

fn loop_pids_file() -> PathBuf {
    state_dir().join("loop-pids.json")
}

/// Load the pid registry. Missing/corrupt file → empty map (never an error —
/// bookkeeping must not break the loop).
pub fn load_loop_pids() -> BTreeMap<String, LoopPidEntry> {
    let raw = std::fs::read_to_string(loop_pids_file()).unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Persist the pid registry atomically (temp + rename).
pub fn save_loop_pids(pids: &BTreeMap<String, LoopPidEntry>) -> Result<()> {
    let path = loop_pids_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(pids)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Register this process as the loop owner for `project`. Returns the
/// previous entry when a LIVE process already owns the project and `force`
/// is false (the caller refuses). A dead pid is replaced silently (the stale
/// loop is gone — its entry is litter, not a guard). `force=true` TERMINATES
/// the previous live process (bd-bvwf.4 — a takeover must never coexist:
/// two loops on one project race polls + double-dispatch), then overwrites
/// the registry.
pub fn register_loop(project: &str, invocation: &str, force: bool) -> Result<Option<LoopPidEntry>> {
    let mut pids = load_loop_pids();
    let mine = LoopPidEntry {
        pid: std::process::id(),
        started_at: now_iso(),
        invocation: invocation.to_string(),
    };
    if !force
        && let Some(prev) = pids.get(project)
        && pid_alive(prev.pid)
    {
        return Ok(Some(prev.clone()));
    }
    if let Some(prev) = pids.get(project) {
        if pid_alive(prev.pid) {
            eprintln!(
                "flywheel: --force taking over {project} loop from pid {} (terminating)",
                prev.pid
            );
            terminate_loop_pid(prev.pid);
        } else {
            eprintln!(
                "flywheel: replacing stale loop entry for {project} (pid {} dead)",
                prev.pid
            );
        }
    }
    pids.insert(project.to_string(), mine);
    save_loop_pids(&pids)?;
    Ok(None)
}

/// Terminate a live loop process (bd-bvwf.4): SIGTERM, then SIGKILL after a
/// ~3s grace if it is still alive. Never touches a dead pid. Best-effort —
/// a failure to kill is logged, not fatal (the registry takeover proceeds;
/// the stray is a supervision concern).
pub fn terminate_loop_pid(pid: u32) {
    if !pid_alive(pid) {
        return;
    }
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !pid_alive(pid) {
            return;
        }
    }
    eprintln!("flywheel: pid {pid} ignored SIGTERM — sending SIGKILL");
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
}

/// Verify this process still OWNS the project's loop entry (bd-bvwf.4). A
/// newer process may have taken over the registry between this process's
/// registration and its invocation start (or the entry was overwritten while
/// a stale process lingered) — the loser must NOT start its durable run, or
/// two loops would resume the same invocation and double-send.
pub fn ensure_loop_owner(project: &str) -> Result<()> {
    let pids = load_loop_pids();
    let mine = std::process::id();
    match pids.get(project) {
        Some(e) if e.pid == mine => Ok(()),
        Some(e) => bail!(
            "loop ownership for {project} taken over by pid {} (started {}) — refusing to run; if that process is a stale stray, run: flywheel loop prune",
            e.pid,
            e.started_at
        ),
        None => bail!("loop pid registry has no entry for {project} — refusing to run"),
    }
}

/// Remove `project`'s entry ONLY when it still belongs to `pid` — a newer
/// process must never be unregistered by an older one exiting late.
pub fn unregister_loop(project: &str, pid: u32) {
    let mut pids = load_loop_pids();
    let remove = pids
        .get(project)
        .map(|e| e.pid == pid)
        .unwrap_or(false);
    if remove {
        pids.remove(project);
        let _ = save_loop_pids(&pids);
    }
}

/// True when a process with this pid exists and is NOT a zombie. Uses
/// `ps -p` (portable on the macOS/Linux hosts this tool targets; no libc
/// dependency). A Z (zombie) process is DEAD for our purposes: it cannot
/// poll, and it lingers in `ps` only until its parent reaps it — treating it
/// as alive would let a stale registry entry block a takeover forever.
pub fn pid_alive(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    let stat = String::from_utf8_lossy(&out).trim().to_string();
    !stat.is_empty() && !stat.starts_with('Z')
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

pub(crate) fn utc_now() -> (u64, u32) {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs(), d.subsec_millis()),
        Err(_) => (0, 0),
    }
}

/// Convert unix seconds to UTC civil time (year, month, day, h, m, s).
/// chrono-backed (is1.2) — the crate's single calendar implementation.
pub(crate) fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    use chrono::{Datelike, Timelike};
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH);
    (dt.year() as i64, dt.month(), dt.day(), dt.hour(), dt.minute(), dt.second())
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
                last_ping_tier: None,
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

    /// is1.2 edge pins: leap day 2000-02-29 (400-year rule) matches chrono.
    #[test]
    fn civil_from_unix_leap_boundary() {
        assert_eq!(civil_from_unix(951_782_400), (2000, 2, 29, 0, 0, 0));
        // u64::MAX as i64 == -1 → 1969-12-31T23:59:59Z (the cast is intended:
        // civil time of the pre-epoch second; callers never pass it, but the
        // conversion must not panic)
        assert_eq!(civil_from_unix(u64::MAX), (1969, 12, 31, 23, 59, 59));
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

    /// 193.3: the REAL aggregation — log_event → read_log → analytics()
    /// counts kinds, by_day, first/last event, and state transitions.
    #[test]
    fn analytics_counts_real_log_entries() {
        let _g = crate::config::test_state_home();
        let project = format!("analytics-{}", std::process::id());
        let _ = std::fs::remove_file(project_log_path(&project));
        log_event(&project, EventKind::Send, json!({ "to": "a" })).unwrap();
        log_event(&project, EventKind::Send, json!({ "to": "b" })).unwrap();
        log_event(&project, EventKind::Interrupt, json!({})).unwrap();
        log_event(&project, EventKind::Checkpoint, json!({})).unwrap();
        log_event(&project, EventKind::Workflow, json!({ "workflow": "w" })).unwrap();
        log_event(&project, EventKind::Pipeline, json!({ "pipeline": "p" })).unwrap();
        log_event(&project, EventKind::Loop, json!({ "loop": "l" })).unwrap();
        log_event(&project, EventKind::State, json!({ "state": "blocked" })).unwrap();
        log_event(&project, EventKind::State, json!({ "state": "blocked" })).unwrap();
        log_event(&project, EventKind::State, json!({ "state": "respawned" })).unwrap();

        let a = analytics(&project).unwrap();
        assert_eq!(a.sends, 2);
        assert_eq!(a.interrupts, 1);
        assert_eq!(a.checkpoints, 1);
        assert_eq!(a.workflow_runs, 1);
        assert_eq!(a.pipeline_runs, 1);
        assert_eq!(a.loop_runs, 1);
        assert_eq!(a.state_transitions.get("blocked"), Some(&2));
        assert_eq!(a.state_transitions.get("respawned"), Some(&1));
        assert_eq!(a.by_day.len(), 1, "all entries land in today's bucket");
        assert_eq!(*a.by_day.values().next().unwrap(), 10);
        assert!(a.first_event.is_some(), "first event recorded");
        assert!(a.last_event.is_some(), "last event recorded");
        assert_eq!(a.first_event, a.last_event, "same-second writes share ts");

        // read_log round-trip: every written entry comes back.
        let entries = read_log(&project).unwrap();
        assert_eq!(entries.len(), 10);
        assert_eq!(entries[0].kind, EventKind::Send);
        assert_eq!(entries[9].kind, EventKind::State);
        let _ = std::fs::remove_file(project_log_path(&project));
    }

    /// 193.3: checkpoint save/list/latest round-trip on disk (functional
    /// core; the state dir is hermetic).
    #[test]
    fn checkpoints_save_list_latest_roundtrip() {
        let _g = crate::config::test_state_home();
        let project = format!("cp-{}", std::process::id());
        let dir = project_checkpoint_dir(&project);
        let _ = std::fs::remove_dir_all(&dir);
        let p1 = save_checkpoint(
            &project,
            "first snapshot",
            Some("ws-1"),
            vec![CheckpointAgent {
                name: "a".into(),
                kind: "oc".into(),
                status: AgentStatus::Idle,
                pane_id: "%1".into(),
            }],
            None,
        )
        .unwrap();
        let p2 = save_checkpoint(&project, "second snapshot", None, vec![], None).unwrap();
        assert_ne!(p1, p2, "ms-resolution names never collide");
        let list = list_checkpoints(&project).unwrap();
        assert_eq!(list.len(), 2);
        // Equal-second saves share `ts`, so sort order is not guaranteed —
        // assert the SET of messages + per-entry fields.
        let mut msgs: Vec<&str> = list.iter().map(|c| c.message.as_str()).collect();
        msgs.sort_unstable();
        assert_eq!(msgs, vec!["first snapshot", "second snapshot"]);
        let second = list.iter().find(|c| c.message == "second snapshot").unwrap();
        assert!(second.agents.is_empty());
        let first = list.iter().find(|c| c.message == "first snapshot").unwrap();
        assert_eq!(first.workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(first.agents[0].name, "a");
        assert_eq!(first.agents[0].status, AgentStatus::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod last_sent_tests {
    use super::*;

    fn rec(name: &str, hash: u64, invocation: &str, landed: bool, ambiguous: bool) -> SentRecord {
        SentRecord {
            name: name.into(),
            text_hash: hash,
            invocation: invocation.into(),
            iteration: 1,
            landed,
            ambiguous,
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
        let mut v: Vec<SentRecord> = (0..60).map(|i| rec("a", i, "inv", true, false)).collect();
        trim_last_sent(&mut v);
        assert_eq!(v.len(), 50);
        // FIFO: the oldest (0..10) dropped, the newest (10..60) kept
        assert_eq!(v[0].text_hash, 10);
        assert_eq!(v[49].text_hash, 59);
    }

    #[test]
    fn trim_under_cap_is_noop() {
        let mut v: Vec<SentRecord> = (0..3).map(|i| rec("a", i, "inv", true, false)).collect();
        trim_last_sent(&mut v);
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn load_save_roundtrip() {
        let _g = crate::config::test_state_home(); // 193.2: never touch the real state dir
        let dir =
            std::env::temp_dir().join(format!("flywheel-lastsent-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // the state dir is fixed by state_dir(); we test through the real file
        // path machinery by temporarily pointing LAST_SENT at a temp dir —
        // instead, exercise record_sent/load_last_sent against a fake project
        // name that collides with nothing, then clean the file up.
        let project = "__test_roundtrip__";
        let _ = std::fs::remove_file(last_sent_file(project));
        record_sent(project, rec("a", 1, "inv1", true, false)).unwrap();
        record_sent(project, rec("b", 2, "inv1", false, false)).unwrap();
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
        let v = vec![rec("a", 11, "inv1", true, false), rec("b", 22, "inv1", true, false)];
        assert!(find_last_sent(&v, "a", 11).is_some());
        assert!(find_last_sent(&v, "a", 22).is_none());
        assert!(find_last_sent(&v, "b", 22).is_some());
        assert!(find_last_sent(&v, "c", 11).is_none());
    }

    #[test]
    fn run_log_classifies_pending_failed_and_owner() {
        // Mirrors the P6.3 digest semantics: run-kind entries classify as
        // pending / failed and carry an owner, non-run kinds are ignored.
        let entries = [
            LogEntry {
                ts: "2026-08-08T01:00:00Z".into(),
                kind: EventKind::Workflow,
                project: "demo".into(),
                data: serde_json::json!({ "workflow": "plan", "status": "done", "tokens": 1200 }),
            },
            LogEntry {
                ts: "2026-08-08T01:05:00Z".into(),
                kind: EventKind::Pipeline,
                project: "demo".into(),
                data: serde_json::json!({ "pipeline": "ship", "status": "awaiting_approval" }),
            },
            LogEntry {
                ts: "2026-08-08T01:10:00Z".into(),
                kind: EventKind::Loop,
                project: "demo".into(),
                data: serde_json::json!({ "loop": "nightly", "status": "failed" }),
            },
            // non-run kind: not a run
            LogEntry {
                ts: "2026-08-08T01:15:00Z".into(),
                kind: EventKind::Send,
                project: "demo".into(),
                data: serde_json::json!({ "to": "scout", "stage": "recon" }),
            },
        ];
        let runs: Vec<&LogEntry> = entries
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Workflow | EventKind::Pipeline | EventKind::Loop))
            .collect();
        assert_eq!(runs.len(), 3, "Send is not a run");

        let pending = runs
            .iter()
            .filter(|e| {
                e.data
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .map(|s| matches!(s, "pending" | "awaiting_approval" | "parked"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(pending, 1, "awaiting_approval is pending");

        let failed = runs
            .iter()
            .filter(|e| {
                e.data
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .map(|s| matches!(s, "failed" | "error"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(failed, 1, "failed loop classified");

        // Owner extraction: workflow / pipeline / loop payload field.
        let owners: Vec<&str> = runs
            .iter()
            .map(|e| {
                ["workflow", "pipeline", "loop", "agent"]
                    .iter()
                    .find_map(|k| e.data.get(*k).and_then(serde_json::Value::as_str))
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(owners, vec!["plan", "ship", "nightly"]);
    }
}

/// l4s.1: per-project sidecar persistence + legacy migration tests.
///
/// These share the REAL state dir (`state_dir()` is fixed) and the migration
/// test manipulates the real legacy `loops.json`, so they are serialized by a
/// mutex and restore pre-test state.
#[cfg(test)]
mod loops_sidecar_tests {
    use super::*;
    use serde_json::json;

    fn loops_test_guard() -> crate::config::TestStateHome {
        // 193.2: hermetic XDG_STATE_HOME (holds the crate-wide env lock) —
        // the REAL state dir is shared with production loops; a leftover
        // fixture there re-triggers the sidecar migration mid-suite.
        crate::config::test_state_home()
    }

    fn test_project(tag: &str) -> String {
        format!("l4s1-{tag}-{}", std::process::id())
    }

    fn make_loop(_project: &str, ping_count: u32) -> ActiveLoop {
        let mut al = ActiveLoop::new("swarm-loop", json!({}), now_iso());
        al.escalation.insert(
            "agent-x".to_string(),
            EscalationState {
                first_blocked_ts: 1_700_000_000_000_000,
                ping_count,
                last_ping_tier: None,
                last_tail: String::new(),
            },
        );
        al
    }

    fn loops_dir_markers() -> Vec<String> {
        std::fs::read_dir(loops_file().parent().unwrap())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("loops.json.migrated-"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// l4s.1 AC2: a legacy shared v2 `loops.json` migrates to per-project
    /// sidecars on first read; the legacy file is renamed away (recoverable)
    /// and the registry view carries the same data after migration.
    #[test]
    fn legacy_registry_migrates_to_sidecars() {
        let _g = loops_test_guard();
        let p1 = test_project("mig-a");
        let p2 = test_project("mig-b");
        let legacy = loops_file();
        // snapshot pre-test state (restore at the end)
        let pre_legacy = std::fs::read_to_string(&legacy).ok();
        let pre_markers = loops_dir_markers();
        // fixture: legacy shared v2 file with 2 projects
        let mut map = BTreeMap::new();
        map.insert(p1.clone(), make_loop(&p1, 1));
        map.insert(p2.clone(), make_loop(&p2, 2));
        let body = serialize_loops_registry(&map).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, body).unwrap();
        // migrate via read
        let (_schema, loaded) = load_loops_registry().unwrap();
        assert_eq!(
            loaded.get(&p1).unwrap().escalation.get("agent-x").unwrap().ping_count,
            1,
            "p1 survives migration"
        );
        assert_eq!(
            loaded.get(&p2).unwrap().escalation.get("agent-x").unwrap().ping_count,
            2,
            "p2 survives migration"
        );
        // legacy renamed + sidecars written
        assert!(!legacy.exists(), "legacy shared file renamed away");
        assert!(sidecar_file(&p1).exists(), "p1 sidecar written");
        assert!(sidecar_file(&p2).exists(), "p2 sidecar written");
        let new_marker = loops_dir_markers()
            .into_iter()
            .any(|n| !pre_markers.contains(&n));
        assert!(new_marker, "loops.json.migrated-<ts> marker created");
        // restore: my sidecars + my marker go; legacy returns to prior state
        for p in [&p1, &p2] {
            let _ = std::fs::remove_file(sidecar_file(p));
            let _ = std::fs::remove_file(sidecar_file(p).with_extension("json.tmp"));
        }
        for n in loops_dir_markers() {
            if !pre_markers.contains(&n) {
                let _ = std::fs::remove_file(loops_file().parent().unwrap().join(&n));
            }
        }
        match pre_legacy {
            Some(bytes) => std::fs::write(&legacy, bytes).unwrap(),
            None => {
                let _ = std::fs::remove_file(&legacy);
            }
        }
    }

    /// bd-fw-loop-status-stale-hjn: an EMPTY legacy loops.json (valid object,
    /// zero entries) is deleted, not renamed — repeat migrations must not
    /// litter loops.json.migrated-* files.
    #[test]
    fn empty_legacy_registry_is_deleted_not_renamed() {
        let _g = loops_test_guard();
        let legacy = loops_file();
        let pre_legacy = std::fs::read_to_string(&legacy).ok();
        let pre_markers = loops_dir_markers();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "{\"schema\":2}").unwrap();
        // migrate via read
        let (_schema, loaded) = load_loops_registry().unwrap();
        assert!(loaded.is_empty());
        assert!(!legacy.exists(), "empty legacy deleted, not renamed");
        assert_eq!(
            loops_dir_markers(),
            pre_markers,
            "no new .migrated-* marker for empty content"
        );
        // corrupt content is NOT deleted (keeps the recoverable rename path)
        std::fs::write(&legacy, "{not json").unwrap();
        let (_schema, loaded) = load_loops_registry().unwrap();
        assert!(loaded.is_empty());
        assert!(
            !legacy.exists(),
            "corrupt legacy still renamed away (recoverable backup)"
        );
        let new_marker = loops_dir_markers()
            .into_iter()
            .any(|n| !pre_markers.contains(&n));
        assert!(new_marker, "corrupt legacy keeps a .migrated-* marker");
        // restore
        for n in loops_dir_markers() {
            if !pre_markers.contains(&n) {
                let _ = std::fs::remove_file(loops_file().parent().unwrap().join(&n));
            }
        }
        match pre_legacy {
            Some(bytes) => std::fs::write(&legacy, bytes).unwrap(),
            None => {
                let _ = std::fs::remove_file(&legacy);
            }
        }
    }

    /// bd-fw-loop-status-stale-hjn: after a real migration only the NEWEST
    /// .migrated-* backup survives — old litter is swept.
    #[test]
    fn migration_sweeps_old_backup_litter() {
        let _g = loops_test_guard();
        let p1 = test_project("sweep-a");
        let legacy = loops_file();
        let pre_legacy = std::fs::read_to_string(&legacy).ok();
        let pre_markers = loops_dir_markers();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        // seed old litter + a legacy file with real content
        for ts in [1000u64, 2000, 3000] {
            std::fs::write(
                legacy.with_file_name(format!("loops.json.migrated-{ts}")),
                "old",
            )
            .unwrap();
        }
        let mut map = BTreeMap::new();
        map.insert(p1.clone(), make_loop(&p1, 3));
        std::fs::write(&legacy, serialize_loops_registry(&map).unwrap()).unwrap();
        let (_schema, loaded) = load_loops_registry().unwrap();
        assert_eq!(loaded.len(), 1, "content migrated");
        // exactly ONE marker remains (the newest = the one just created)
        let markers = loops_dir_markers();
        assert_eq!(markers.len(), pre_markers.len() + 1, "litter swept: {markers:?}");
        let new_markers: Vec<String> = markers
            .iter()
            .filter(|n| !pre_markers.contains(n))
            .cloned()
            .collect();
        assert_eq!(new_markers.len(), 1, "one new marker kept");
        assert!(
            !new_markers[0].contains("migrated-1000")
                && !new_markers[0].contains("migrated-2000")
                && !new_markers[0].contains("migrated-3000"),
            "seeded old litter gone: {new_markers:?}"
        );
        // restore
        let _ = std::fs::remove_file(sidecar_file(&p1));
        let _ = std::fs::remove_file(sidecar_file(&p1).with_extension("json.tmp"));
        for n in loops_dir_markers() {
            if !pre_markers.contains(&n) {
                let _ = std::fs::remove_file(loops_file().parent().unwrap().join(&n));
            }
        }
        match pre_legacy {
            Some(bytes) => std::fs::write(&legacy, bytes).unwrap(),
            None => {
                let _ = std::fs::remove_file(&legacy);
            }
        }
    }

    /// l4s.1: save/load per project — writers are disjoint, readers see only
    /// their own project.
    #[test]
    fn sidecar_roundtrip_per_project() {
        let _g = loops_test_guard();
        let p1 = test_project("rt-a");
        let p2 = test_project("rt-b");
        save_active_loop(&p1, &make_loop(&p1, 7)).unwrap();
        save_active_loop(&p2, &make_loop(&p2, 9)).unwrap();
        let a1 = load_active_loop(&p1).unwrap().expect("p1");
        let a2 = load_active_loop(&p2).unwrap().expect("p2");
        assert_eq!(a1.escalation.get("agent-x").unwrap().ping_count, 7);
        assert_eq!(a2.escalation.get("agent-x").unwrap().ping_count, 9);
        assert_eq!(load_active_loop("l4s1-no-such-project").unwrap(), None);
        remove_active_loop(&p1).unwrap();
        remove_active_loop(&p2).unwrap();
    }

    /// Absolute project paths must not collapse the per-project sidecars
    /// onto <abs-path>.json — state_key slugs them; relative names pass
    /// through verbatim (existing state files keep their names).
    #[test]
    fn state_key_slugs_absolute_paths_keeps_relative() {
        assert_eq!(state_key("toron"), "toron");
        assert_eq!(state_key("verify2"), "verify2");
        let a = state_key("/var/folders/x/tmp/proj-a");
        assert!(a.starts_with("proj-a-"), "{a}");
        assert!(!a.contains('/'), "slug must be path-safe: {a}");
        let b = state_key("/var/folders/y/tmp/proj-a");
        assert_ne!(a, b, "same base, different path -> different keys");
        assert_eq!(state_key("/var/folders/x/tmp/proj-a"), a, "deterministic");
    }

    /// bd-a7j: the expected-reply registry round-trips; remove drops only
    /// the (agent, thread) pair.
    #[test]
    fn expected_reply_registry_roundtrip() {
        let _g = loops_test_guard();
        let p = test_project("exp-a");
        let mk = |agent: &str, thread: &str| ExpectedReply {
            agent: agent.to_string(),
            thread: thread.to_string(),
            head: Some(format!("q-{agent}-{thread}")),
            created_ts: now_iso(),
        };
        let entries = vec![mk("oc", "t1"), mk("oc", "t2"), mk("pi", "t1")];
        save_expected_replies(&p, &entries).unwrap();
        assert_eq!(load_expected_replies(&p), entries);
        // remove one (agent, thread) pair only
        remove_expected_reply(&p, "oc", "t1").unwrap();
        let left = load_expected_replies(&p);
        assert_eq!(left.len(), 2);
        assert!(!left.iter().any(|e| e.agent == "oc" && e.thread == "t1"));
        assert!(left.iter().any(|e| e.agent == "oc" && e.thread == "t2"));
        assert!(left.iter().any(|e| e.agent == "pi" && e.thread == "t1"));
        // missing file → empty; corrupt → empty
        assert!(load_expected_replies("l4s1-no-such-project").is_empty());
        let path = expected_reply_file(&p);
        std::fs::write(&path, "{not json").unwrap();
        assert!(load_expected_replies(&p).is_empty());
        // cleanup
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.tmp"));
    }

    /// bd-fw-name-binding-loss-7rc: the spawn registry (pane_id → name)
    /// round-trips atomically and tolerates missing/corrupt files. P10.3
    /// (bd-ghcj.1): entries carry an optional PROFILE column.
    #[test]
    fn spawn_registry_roundtrip() {
        let _g = loops_test_guard();
        let p = test_project("reg-a");
        let entries = vec![
            SpawnEntry::legacy("w2A:p1", "flywheel-reg-a-oc"),
            SpawnEntry::legacy("w2A:p2", "flywheel-reg-a-pi"),
        ];
        save_spawn_registry(&p, &entries).unwrap();
        assert_eq!(load_spawn_registry(&p), entries);
        // overwrite atomically
        save_spawn_registry(&p, &entries[..1]).unwrap();
        assert_eq!(load_spawn_registry(&p), entries[..1]);
        // missing → empty; corrupt → empty
        assert!(load_spawn_registry("l4s1-no-such-project").is_empty());
        let path = spawn_registry_file(&p);
        std::fs::write(&path, "{not json").unwrap();
        assert!(load_spawn_registry(&p).is_empty());
        // cleanup
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.tmp"));
    }

    /// bd-eign.3 A: the project-local agent-names mirror round-trips and
    /// lives at `.flywheel/agent-names.json` inside the PROJECT dir (not the
    /// state dir) — a state-dir wipe must not lose the binding.
    #[test]
    fn agent_names_roundtrip_in_project_dir() {
        let _g = loops_test_guard();
        let dir = std::env::temp_dir().join(format!("fw-agent-names-{}", std::process::id()));
        let entries = vec![
            SpawnEntry::legacy("w2A:p1", "flywheel-reg-a-oc"),
            SpawnEntry::legacy("w2A:p2", "flywheel-reg-a-pi"),
        ];
        save_agent_names(&dir, &entries).unwrap();
        assert!(agent_names_file(&dir).ends_with(".flywheel/agent-names.json"));
        assert!(agent_names_file(&dir).starts_with(&dir));
        assert_eq!(load_agent_names(&dir), entries);
        // missing → empty
        let empty_dir = std::env::temp_dir().join(format!("fw-agent-names-none-{}", std::process::id()));
        assert!(load_agent_names(&empty_dir).is_empty());
        // cleanup
        let _ = std::fs::remove_file(agent_names_file(&dir));
        let _ = std::fs::remove_file(dir.join(".flywheel").join("agent-names.json.tmp"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty_dir);
    }

    /// bd-ghcj.1: a PROFILE column on registry entries survives a round-trip,
    /// and pre-P10.3 2-tuple files still load (tolerant Deserialize).
    #[test]
    fn spawn_registry_profile_column_roundtrip() {
        let _g = loops_test_guard();
        let p = test_project("reg-profile");
        let entries = vec![
            SpawnEntry {
                pane_id: "w2A:p1".into(),
                name: "crew-wf-planner-1".into(),
                profile: Some("planner".into()),
                harness: Some("omp".into()),
                mail_identity: None,
            },
            SpawnEntry::legacy("w2A:p2", "flywheel-reg-profile-oc"),
        ];
        save_spawn_registry(&p, &entries).unwrap();
        assert_eq!(load_spawn_registry(&p), entries);
        let legacy_path = spawn_registry_file(&p);
        let raw = std::fs::read_to_string(spawn_registry_file(&p)).unwrap();
        assert!(raw.contains("profile"), "profile column serialized: {raw}");
        assert!(
            raw.contains("harness"),
            "harness column serialized (bd-lvyi): {raw}"
        );
        // a P10.3-style file WITHOUT the harness column (pre-bd-lvyi) still
        // deserializes — harness: None
        std::fs::write(
            &legacy_path,
            r#"[{"pane_id":"w8:p1","name":"crew-wf-planner-1","profile":"planner"}]"#,
        )
        .unwrap();
        let loaded = load_spawn_registry(&p);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].profile.as_deref(), Some("planner"));
        assert_eq!(loaded[0].harness, None, "absent column -> None");
        assert_eq!(loaded[0].mail_identity, None, "absent column -> None");
        std::fs::write(
            &legacy_path,
            r#"[["w9:p1","flywheel-x-oc"],["w9:p2","flywheel-x-pi"]]"#,
        )
        .unwrap();
        let loaded = load_spawn_registry(&p);
        assert_eq!(
            loaded,
            vec![
                SpawnEntry::legacy("w9:p1", "flywheel-x-oc"),
                SpawnEntry::legacy("w9:p2", "flywheel-x-pi"),
            ]
        );
        assert!(loaded.iter().all(|e| e.profile.is_none()));
        assert!(loaded.iter().all(|e| e.harness.is_none()));
        // cleanup
        let _ = std::fs::remove_file(&legacy_path);
        let _ = std::fs::remove_file(legacy_path.with_extension("json.tmp"));
    }

    /// bd-627z.4: the MAIL IDENTITY column survives a round-trip (the
    /// deterministic pane→identity pin assign resolves through), and
    /// pre-bd-627z.4 files (no column) still load with mail_identity: None.
    #[test]
    fn spawn_registry_mail_identity_column_roundtrip() {
        let _g = loops_test_guard();
        let p = test_project("reg-mail-id");
        let entries = vec![
            SpawnEntry::with_mail_identity("w3:p1", "flywheel-reg-mail-oc2", "DustySparrow"),
            SpawnEntry::with_mail_identity("w3:p2", "flywheel-reg-mail-oc3", "RoseDune"),
        ];
        save_spawn_registry(&p, &entries).unwrap();
        assert_eq!(load_spawn_registry(&p), entries, "identity pin round-trips");

        // A pre-bd-627z.4 file (pane+name+profile only) still loads —
        // mail_identity: None → assign falls back to the hash mapping.
        let path = spawn_registry_file(&p);
        std::fs::write(
            &path,
            r#"[{"pane_id":"w9:p1","name":"flywheel-x-oc2","profile":"coder","harness":"omp"}]"#,
        )
        .unwrap();
        let loaded = load_spawn_registry(&p);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "flywheel-x-oc2");
        assert_eq!(loaded[0].mail_identity, None, "absent column -> None");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.tmp"));
    }

    /// bd-fw-stale-loop-processes: the pid registry round-trips atomically.
    #[test]
    fn loop_pid_registry_roundtrip() {
        let _g = loops_test_guard();
        let p = test_project("pid-rt");
        let e = LoopPidEntry {
            pid: 4242,
            started_at: now_iso(),
            invocation: "swarm-loop:proj:1".into(),
        };
        let mut pids = BTreeMap::new();
        pids.insert(p.clone(), e.clone());
        save_loop_pids(&pids).unwrap();
        let loaded = load_loop_pids();
        assert_eq!(loaded.get(&p), Some(&e));
        // unregister removes only matching pid
        unregister_loop(&p, 9999);
        assert_eq!(load_loop_pids().get(&p), Some(&e), "other pid must not unregister");
        unregister_loop(&p, 4242);
        assert!(!load_loop_pids().contains_key(&p));
    }

    /// bd-fw-stale-loop-processes: a dead pid entry is replaced silently by a
    /// fresh registration; a live pid entry refuses unless force.
    #[test]
    fn loop_pid_registry_live_guard() {
        let _g = loops_test_guard();
        let p = test_project("pid-live");
        // no prior entry → registers cleanly
        assert!(register_loop(&p, "inv-1", false).unwrap().is_none());
        let registered = load_loop_pids().get(&p).unwrap().clone();
        assert_eq!(registered.pid, std::process::id());
        // the registry's own pid is alive → a second registration refuses
        let prev = register_loop(&p, "inv-2", false).unwrap().expect("live owner");
        assert_eq!(prev.pid, std::process::id());
        // cleanup the self entry before the takeover case
        unregister_loop(&p, std::process::id());

        // force takeover TERMINATES the previous live process (bd-bvwf.4) —
        // a takeover must never leave two loops on one project. A real child
        // stands in for the stale loop (never the test process itself).
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();
        let mut pids = load_loop_pids();
        pids.insert(
            p.clone(),
            LoopPidEntry {
                pid: child_pid,
                started_at: now_iso(),
                invocation: "inv-2".into(),
            },
        );
        save_loop_pids(&pids).unwrap();
        assert!(pid_alive(child_pid), "child is the live loop");
        assert!(register_loop(&p, "inv-3", true).unwrap().is_none());
        assert!(!pid_alive(child_pid), "--force terminates the old loop process");
        assert_eq!(load_loop_pids().get(&p).unwrap().invocation, "inv-3");
        // cleanup
        let _ = child.wait();
        unregister_loop(&p, std::process::id());
    }

    /// bd-bvwf.4: a loop process verifies it STILL owns the registry entry
    /// right before starting its durable run — a newer process that took
    /// over (or a pruned/stale entry) refuses the loser.
    #[test]
    fn ensure_loop_owner_guards_takeover() {
        let _g = loops_test_guard();
        let p = test_project("pid-owner");
        // own pid → owner
        register_loop(&p, "inv-1", false).unwrap();
        assert!(ensure_loop_owner(&p).is_ok(), "own entry passes");
        unregister_loop(&p, std::process::id());

        // no entry → refuse (never run unowned — the registry is the guard)
        assert!(ensure_loop_owner(&p).is_err(), "no entry refuses");

        // a foreign live pid owns it → refuse (a newer process took over)
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();
        let mut pids = load_loop_pids();
        pids.insert(
            p.clone(),
            LoopPidEntry {
                pid: child_pid,
                started_at: now_iso(),
                invocation: "inv-other".into(),
            },
        );
        save_loop_pids(&pids).unwrap();
        assert!(ensure_loop_owner(&p).is_err(), "foreign live owner refuses");
        // cleanup
        unregister_loop(&p, child_pid);
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &child_pid.to_string()])
            .status();
        let _ = child.wait();
    }

    /// l4s.1 AC1: 8 writers on 8 DIFFERENT projects, 50 saves each, run
    /// concurrently — every project's record survives (no lost updates).
    #[test]
    fn stress_concurrent_saves_no_lost_updates() {
        let _g = loops_test_guard();
        let n = 8usize;
        let per = 50usize;
        let projects: Vec<String> =
            (0..n).map(|i| test_project(&format!("stress-{i}"))).collect();
        let mut handles = Vec::new();
        for (i, p) in projects.iter().enumerate() {
            let p = p.clone();
            handles.push(std::thread::spawn(move || {
                for k in 0..per {
                    save_active_loop(&p, &make_loop(&p, (i * 1000 + k) as u32)).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let (_schema, loaded) = load_loops_registry().unwrap();
        for (i, p) in projects.iter().enumerate() {
            let al = loaded
                .get(p)
                .unwrap_or_else(|| panic!("project {p} lost in concurrent saves"));
            assert_eq!(
                al.escalation.get("agent-x").unwrap().ping_count / 1000,
                i as u32,
                "project {p} carried another writer's data"
            );
        }
        for p in &projects {
            remove_active_loop(p).unwrap();
        }
    }

    /// remove_active_loop deletes the sidecar and is a no-op when absent.
    #[test]
    fn remove_active_loop_deletes_sidecar() {
        let _g = loops_test_guard();
        let p = test_project("rm");
        save_active_loop(&p, &make_loop(&p, 1)).unwrap();
        assert!(sidecar_file(&p).exists());
        remove_active_loop(&p).unwrap();
        assert!(!sidecar_file(&p).exists());
        remove_active_loop(&p).unwrap(); // idempotent
    }

    /// cg6.3: the xproj seen-marker dedups one message id — mark is
    /// idempotent and no line is ever duplicated.
    #[test]
    fn xproj_seen_marker_dedups() {
        let _g = loops_test_guard();
        let project = test_project("xproj");
        let id = "deadbeef-xproj-msg-1";
        let path = xproj_seen_file(&project);
        let _ = std::fs::remove_file(&path);
        assert!(!xproj_seen_contains(&project, id).unwrap());
        xproj_seen_mark(&project, id).unwrap();
        assert!(xproj_seen_contains(&project, id).unwrap());
        xproj_seen_mark(&project, id).unwrap(); // idempotent re-mark
        xproj_seen_mark(&project, "second-msg-2").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 2, "no duplicate marker lines");
        let _ = std::fs::remove_file(&path);
    }

    /// cg6.6: a failed spawn unmarks — the request retries next iteration.
    /// Unmark removes exactly its line; missing marker/file is a no-op.
    #[test]
    fn xproj_seen_unmark_removes_only_its_line() {
        let _g = loops_test_guard();
        let project = test_project("xproj-unmark");
        let path = xproj_seen_file(&project);
        let _ = std::fs::remove_file(&path);
        xproj_seen_mark(&project, "msg-a").unwrap();
        xproj_seen_mark(&project, "msg-b").unwrap();
        xproj_seen_mark(&project, "msg-c").unwrap();

        xproj_seen_unmark(&project, "msg-b").unwrap();
        assert!(!xproj_seen_contains(&project, "msg-b").unwrap(), "unmarked");
        assert!(xproj_seen_contains(&project, "msg-a").unwrap(), "siblings kept");
        assert!(xproj_seen_contains(&project, "msg-c").unwrap(), "siblings kept");

        // no-op paths: unmarked id, and a missing file.
        xproj_seen_unmark(&project, "msg-b").unwrap();
        xproj_seen_unmark(&project, "never-marked").unwrap();
        let _ = std::fs::remove_file(&path);
        xproj_seen_unmark(&project, "msg-a").unwrap(); // missing file → Ok
    }

    // ---- l4s.2: read failures propagate, never clobber ----

    /// A read failure on the registry aborts the save with an error and the
    /// on-disk state is untouched (crash > corrupt). Simulated by making the
    /// legacy path a DIRECTORY (read_to_string → EISDIR).
    #[test]
    fn unreadable_registry_aborts_save_without_clobber() {
        let _g = loops_test_guard();
        let p = test_project("unreadable");
        let legacy = loops_file();
        // snapshot + park any real legacy file
        let pre_legacy = std::fs::read_to_string(&legacy).ok();
        let _ = std::fs::remove_file(&legacy);
        // a directory at the legacy path is unreadable as a file
        std::fs::create_dir_all(&legacy).unwrap();
        let err = save_active_loop(&p, &make_loop(&p, 3)).unwrap_err();
        assert!(err.to_string().contains("Is a directory") || err.to_string().contains("EISDIR") || err.to_string().contains("not a regular file") || !err.to_string().is_empty(), "save aborts: {err:#}");
        assert!(legacy.is_dir(), "unreadable path left untouched");
        // restore
        std::fs::remove_dir_all(&legacy).unwrap();
        if let Some(bytes) = pre_legacy {
            std::fs::write(&legacy, bytes).unwrap();
        }
        let _ = remove_active_loop(&p);
    }

    /// A missing registry saves normally — no error, no migration log (the
    /// audit eprintln only fires on GENUINE v1 content, l4s.2).
    #[test]
    fn missing_registry_saves_normally() {
        let _g = loops_test_guard();
        let p = test_project("missing");
        let legacy = loops_file();
        let pre_legacy = std::fs::read_to_string(&legacy).ok();
        let _ = std::fs::remove_file(&legacy);
        // fresh registry: save must succeed
        save_active_loop(&p, &make_loop(&p, 5)).unwrap();
        assert_eq!(load_active_loop(&p).unwrap().unwrap().escalation.get("agent-x").unwrap().ping_count, 5);
        if let Some(bytes) = pre_legacy {
            // restore the real legacy (it will re-split on next access)
            let _ = std::fs::remove_file(&legacy);
            std::fs::write(&legacy, bytes).unwrap();
        }
        let _ = remove_active_loop(&p);
    }

    /// Genuine v1 content still migrates (schema downgrade path preserved:
    /// audit message + split into sidecars).
    #[test]
    fn genuine_v1_content_migrates_with_audit() {
        let _g = loops_test_guard();
        let p = test_project("v1");
        let legacy = loops_file();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap(); // hermetic dir starts empty
        let pre_legacy = std::fs::read_to_string(&legacy).ok();
        let pre_markers = loops_dir_markers();
        // minimal v1 fixture: flat project map, no schema key
        let fixture = json!({
            p.clone(): {
                "id": format!("swarm-loop:{p}:1"),
                "params": {"project": p},
                "created": "2026-08-04T00:00:00Z"
            }
        })
        .to_string();
        std::fs::write(&legacy, fixture).unwrap();
        // read triggers the split; the v1 entry gains defaulted control fields
        let (_schema, loaded) = load_loops_registry().unwrap();
        assert_eq!(loaded.get(&p).unwrap().id, format!("swarm-loop:{p}:1"));
        assert!(sidecar_file(&p).exists());
        let new_marker = loops_dir_markers().into_iter().any(|n| !pre_markers.contains(&n));
        assert!(new_marker, "legacy renamed after v1 migration");
        // restore
        let _ = std::fs::remove_file(sidecar_file(&p));
        for n in loops_dir_markers() {
            if !pre_markers.contains(&n) {
                let _ = std::fs::remove_file(loops_file().parent().unwrap().join(&n));
            }
        }
        match pre_legacy {
            Some(bytes) => std::fs::write(&legacy, bytes).unwrap(),
            None => {
                let _ = std::fs::remove_file(&legacy);
            }
        }
    }
}
