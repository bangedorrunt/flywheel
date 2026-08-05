//! herdr backend client — newline-delimited JSON-RPC over a unix socket.
//!
//! Protocol: each request is one JSON object per line:
//!   {"id": "...", "method": "agent.prompt", "params": {...}}
//! Responses match the request id:
//!   {"id": "...", "result": {...}}  |  {"id": "...", "error": {"code", "message"}}
//! Pushed notifications (events) arrive as:
//!   {"event": "<type>", "data": {...}}
//!
//! Verified against herdr 0.8.0-preview (socket protocol v19, schema version 1),
//! preview build 2026-08-04-d78e3d3b5126. Canonical verification source:
//! `herdr api schema --json` (fields: protocol, schema_version).

/// The herdr socket protocol version this client is verified against —
/// single source of truth (bd-deps-protocol-check). `flywheel deps` warns
/// when the live server reports a different value; a bump that adds required
/// fields would break the client at runtime, and the WARN line turns a
/// confusing mid-loop failure into a five-second diagnosis.
pub const HDR_PROTOCOL: u64 = 19;

/// Parse the protocol version from `herdr api schema --json` output (the
/// top-level `protocol` field). `None` on any failure — the caller decides
/// how to surface an unreadable schema.
pub fn parse_protocol(raw: &str) -> Option<u64> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("protocol").and_then(Value::as_u64))
}

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "idle" => Ok(AgentStatus::Idle),
            "working" => Ok(AgentStatus::Working),
            "blocked" => Ok(AgentStatus::Blocked),
            "done" => Ok(AgentStatus::Done),
            "unknown" => Ok(AgentStatus::Unknown),
            other => bail!("unknown agent status: {other}"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
// wire-contract struct: parsed from the herdr protocol; not every
// field is consumed by the client — the unused ones must still deserialize.
#[allow(dead_code)]
pub struct AgentSessionInfo {
    pub agent: String,
    pub kind: String,
    pub source: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
// wire-contract struct: parsed from the herdr protocol; not every
// field is consumed by the client — the unused ones must still deserialize.
#[allow(dead_code)]
pub struct AgentInfo {
    pub agent: Option<String>,
    pub agent_status: AgentStatus,
    pub cwd: Option<String>,
    pub display_agent: Option<String>,
    pub focused: bool,
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub interactive_ready: bool,
    #[serde(default)]
    pub launch_pending: bool,
    pub name: Option<String>,
    pub pane_id: String,
    pub revision: u64,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub state_labels: serde_json::Map<String, Value>,
    pub tab_id: String,
    pub terminal_id: String,
    pub terminal_title: Option<String>,
    pub workspace_id: String,
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
}

#[derive(Debug, Clone, Deserialize)]
// wire-contract struct: parsed from the herdr protocol; not every
// field is consumed by the client — the unused ones must still deserialize.
#[allow(dead_code)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub number: u64,
    pub label: String,
    pub focused: bool,
    pub pane_count: u64,
    pub tab_count: u64,
    pub active_tab_id: String,
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Resolve which of several same-label workspaces a project actually maps
/// to. Single match wins; with duplicates (a stale workspace left behind by
/// a dropped agent vs the live spawn — bd-nil), prefer the workspace holding
/// MANAGED agents (names starting with `prefix`); otherwise the first match.
/// Deterministic — no wall clock in the durable path.
pub fn resolve_workspace_id(
    matches: &[WorkspaceInfo],
    agents: &[AgentInfo],
    prefix: &str,
) -> String {
    if matches.len() == 1 {
        return matches[0].workspace_id.clone();
    }
    for ws in matches {
        if agents.iter().any(|a| {
            a.workspace_id == ws.workspace_id
                && a.name
                    .as_deref()
                    .map(|n| n.starts_with(prefix))
                    .unwrap_or(false)
        }) {
            return ws.workspace_id.clone();
        }
    }
    matches[0].workspace_id.clone()
}

#[derive(Debug, Clone, Deserialize)]
// wire-contract struct: parsed from the herdr protocol; not every
// field is consumed by the client — the unused ones must still deserialize.
#[allow(dead_code)]
pub struct PaneInfo {
    pub pane_id: String,
    pub terminal_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    pub focused: bool,
    pub cwd: Option<String>,
    pub agent_status: AgentStatus,
    pub revision: u64,
}

#[derive(Debug, Clone, Deserialize)]
// wire-contract struct: parsed from the herdr protocol; not every
// field is consumed by the client — the unused ones must still deserialize.
#[allow(dead_code)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
    pub is_linked_worktree: bool,
    pub open_workspace_id: Option<String>,
    pub label: String,
}

/// Push notification pushed by the server (event subscription or server events).
///
/// Typed: `kind` is the wire event name parsed into `HerdrEventKind`, and
/// `data` is deserialized into the matching typed payload — no `Value`
/// leakage in the fields we route on (`agent_status`, `pane_id`, `agent`,
/// `workspace_id`). Deserialization is implemented manually because the wire
/// carries the kind in the envelope (`event`) while the payloads use
/// per-variant content shapes (see `HerdrEventData::from_kind`).
#[derive(Debug, Clone)]
pub struct PushEvent {
    #[allow(dead_code)] // bin matches on the typed `data` variant; `kind` is the typed wire name
    pub kind: HerdrEventKind,
    pub data: HerdrEventData,
}

impl<'de> Deserialize<'de> for PushEvent {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let v = Value::deserialize(d)?;
        let kind_str = v
            .get("event")
            .and_then(Value::as_str)
            .ok_or_else(|| D::Error::custom("PushEvent missing 'event' field"))?;
        let kind = HerdrEventKind::parse(kind_str)
            .map_err(|_| D::Error::custom(format!("unknown herdr event kind: {kind_str}")))?;
        let data = HerdrEventData::from_kind(kind, v.get("data").cloned().unwrap_or(Value::Null))
            .map_err(D::Error::custom)?;
        Ok(PushEvent { kind, data })
    }
}

/// All herdr event kinds: the 26 server events (snake_case wire names) plus
/// the 3 subscription pushes (dot namespaced wire names). `parse` accepts
/// both namespaces; `as_wire_str` returns the exact wire name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // consumed by tests + the Phase-2 reactor; bin matches on data today
pub enum HerdrEventKind {
    // server events (26) — snake_case on the wire
    WorkspaceCreated,
    WorkspaceUpdated,
    WorkspaceMetadataUpdated,
    WorkspaceClosed,
    WorkspaceRenamed,
    WorkspaceMoved,
    WorkspaceReordered,
    WorkspaceFocused,
    WorktreeCreated,
    WorktreeOpened,
    WorktreeRemoved,
    TabCreated,
    TabClosed,
    TabRenamed,
    TabMoved,
    TabFocused,
    PaneCreated,
    PaneClosed,
    PaneUpdated,
    PaneFocused,
    PaneMoved,
    PaneOutputChanged,
    PaneExited,
    PaneAgentDetected,
    PaneAgentStatusChanged,
    LayoutUpdated,
    // subscription pushes (3) — dotted on the wire
    PaneAgentStatusChangedSub,
    PaneOutputMatched,
    PaneScrollChanged,
}

impl HerdrEventKind {
    /// Parse a wire event name. Accepts both namespaces: the 26 snake_case
    /// server events and the 3 dotted subscription events.
    #[allow(dead_code)] // used by tests + reactor; parse itself is live via PushEvent::deserialize
    pub fn parse(s: &str) -> Result<Self> {
        use HerdrEventKind::*;
        let kind = match s {
            "workspace_created" => WorkspaceCreated,
            "workspace_updated" => WorkspaceUpdated,
            "workspace_metadata_updated" => WorkspaceMetadataUpdated,
            "workspace_closed" => WorkspaceClosed,
            "workspace_renamed" => WorkspaceRenamed,
            "workspace_moved" => WorkspaceMoved,
            "workspace_reordered" => WorkspaceReordered,
            "workspace_focused" => WorkspaceFocused,
            "worktree_created" => WorktreeCreated,
            "worktree_opened" => WorktreeOpened,
            "worktree_removed" => WorktreeRemoved,
            "tab_created" => TabCreated,
            "tab_closed" => TabClosed,
            "tab_renamed" => TabRenamed,
            "tab_moved" => TabMoved,
            "tab_focused" => TabFocused,
            "pane_created" => PaneCreated,
            "pane_closed" => PaneClosed,
            "pane_updated" => PaneUpdated,
            "pane_focused" => PaneFocused,
            "pane_moved" => PaneMoved,
            "pane_output_changed" => PaneOutputChanged,
            "pane_exited" => PaneExited,
            "pane_agent_detected" => PaneAgentDetected,
            "pane_agent_status_changed" => PaneAgentStatusChanged,
            "layout_updated" => LayoutUpdated,
            "pane.agent_status_changed" => PaneAgentStatusChangedSub,
            "pane.output_matched" => PaneOutputMatched,
            "pane.scroll_changed" => PaneScrollChanged,
            other => bail!("unknown herdr event kind: {other}"),
        };
        Ok(kind)
    }

    /// The exact wire name for this kind.
    #[allow(dead_code)] // used by tests + reactor (diagnostics)
    pub fn as_wire_str(self) -> &'static str {
        use HerdrEventKind::*;
        match self {
            WorkspaceCreated => "workspace_created",
            WorkspaceUpdated => "workspace_updated",
            WorkspaceMetadataUpdated => "workspace_metadata_updated",
            WorkspaceClosed => "workspace_closed",
            WorkspaceRenamed => "workspace_renamed",
            WorkspaceMoved => "workspace_moved",
            WorkspaceReordered => "workspace_reordered",
            WorkspaceFocused => "workspace_focused",
            WorktreeCreated => "worktree_created",
            WorktreeOpened => "worktree_opened",
            WorktreeRemoved => "worktree_removed",
            TabCreated => "tab_created",
            TabClosed => "tab_closed",
            TabRenamed => "tab_renamed",
            TabMoved => "tab_moved",
            TabFocused => "tab_focused",
            PaneCreated => "pane_created",
            PaneClosed => "pane_closed",
            PaneUpdated => "pane_updated",
            PaneFocused => "pane_focused",
            PaneMoved => "pane_moved",
            PaneOutputChanged => "pane_output_changed",
            PaneExited => "pane_exited",
            PaneAgentDetected => "pane_agent_detected",
            PaneAgentStatusChanged => "pane_agent_status_changed",
            LayoutUpdated => "layout_updated",
            PaneAgentStatusChangedSub => "pane.agent_status_changed",
            PaneOutputMatched => "pane.output_matched",
            PaneScrollChanged => "pane.scroll_changed",
        }
    }
}

// ---- typed event payloads ----
//
// Field sets mirror the live schema (`herdr api schema --json` →
// `schemas/event/$defs/EventData.oneOf`). Nested objects that flywheel does
// not consume yet stay `Value` — the goal is type safety for the fields we
// route on, not a full model of every payload. `WorkspaceInfo`, `PaneInfo`
// and `WorktreeInfo` (defined above) are reused for the nested payloads.

/// tab payload (nested object; consumed fields only).
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct TabInfo {
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub pane_count: u64,
    #[serde(default)]
    pub agent_status: Option<AgentStatus>,
}

/// scroll payload for `pane.scroll_changed` subscription events.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[allow(dead_code)]
pub struct PaneScrollInfo {
    #[serde(default)]
    pub offset_from_bottom: u64,
    #[serde(default)]
    pub max_offset_from_bottom: u64,
    #[serde(default)]
    pub viewport_rows: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorkspaceCreated {
    pub workspace: WorkspaceInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorkspaceUpdated {
    pub workspace: WorkspaceInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorkspaceMetadataUpdated {
    pub workspace: WorkspaceInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorkspaceClosed {
    pub workspace_id: String,
    #[serde(default)]
    pub workspace: Option<WorkspaceInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorkspaceRenamed {
    pub workspace_id: String,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorkspaceMoved {
    pub workspace_id: String,
    pub insert_index: u64,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorkspaceReordered {
    #[serde(default)]
    pub workspace_ids: Vec<String>,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceInfo>,
    #[serde(default)]
    pub before_workspace_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorkspaceFocused {
    pub workspace_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorktreeCreated {
    pub workspace: WorkspaceInfo,
    pub worktree: WorktreeInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorktreeOpened {
    pub workspace: WorkspaceInfo,
    pub worktree: WorktreeInfo,
    #[serde(default)]
    pub already_open: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorktreeRemoved {
    pub workspace_id: String,
    #[serde(default)]
    pub workspace: Option<WorkspaceInfo>,
    pub worktree: WorktreeInfo,
    #[serde(default)]
    pub forced: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct TabCreated {
    pub tab: TabInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct TabClosed {
    pub tab_id: String,
    pub workspace_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct TabRenamed {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct TabMoved {
    pub tab_id: String,
    pub workspace_id: String,
    pub insert_index: u64,
    #[serde(default)]
    pub tabs: Vec<TabInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct TabFocused {
    pub tab_id: String,
    pub workspace_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneCreated {
    pub pane: PaneInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneClosed {
    pub pane_id: String,
    pub workspace_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneUpdated {
    pub pane: PaneInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneFocused {
    pub pane_id: String,
    pub workspace_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneMoved {
    pub pane: PaneInfo,
    pub previous_pane_id: String,
    pub previous_workspace_id: String,
    pub previous_tab_id: String,
    #[serde(default)]
    pub closed_tab_id: Option<String>,
    #[serde(default)]
    pub closed_workspace_id: Option<String>,
    #[serde(default)]
    pub created_tab: Option<TabInfo>,
    #[serde(default)]
    pub created_workspace: Option<WorkspaceInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneOutputChanged {
    pub pane_id: String,
    pub workspace_id: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneExited {
    pub pane_id: String,
    pub workspace_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneAgentDetected {
    pub pane_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub final_status: Option<AgentStatus>,
    #[serde(default)]
    pub released: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
// wire-contract struct: parsed from the herdr protocol; not every
// field is consumed by the client — the unused ones must still deserialize.
#[allow(dead_code)]
pub struct PaneAgentStatusChanged {
    pub pane_id: String,
    pub workspace_id: String,
    pub agent_status: AgentStatus,
    pub agent: Option<String>,
    #[serde(default)]
    pub display_agent: Option<String>,
    #[serde(default)]
    pub state_labels: Option<Value>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct LayoutUpdated {
    // not consumed yet — type `PaneLayoutSnapshot` when the reactor lands
    #[serde(default)]
    pub layout: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneOutputMatchedEvent {
    pub pane_id: String,
    #[serde(default)]
    pub matched_line: Option<String>,
    #[serde(default)]
    pub read: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneScrollChangedEvent {
    pub pane_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub scroll: PaneScrollInfo,
}

/// Typed payload of a `PushEvent`, dispatched on the envelope's `kind`.
#[derive(Debug, Clone)]
#[allow(dead_code)] // variant payloads consumed by tests + Phase-2 reactor
#[allow(clippy::large_enum_variant, clippy::enum_variant_names)] // wire-fidelity names; parsed once per event
pub enum HerdrEventData {
    WorkspaceCreated(WorkspaceCreated),
    WorkspaceUpdated(WorkspaceUpdated),
    WorkspaceMetadataUpdated(WorkspaceMetadataUpdated),
    WorkspaceClosed(WorkspaceClosed),
    WorkspaceRenamed(WorkspaceRenamed),
    WorkspaceMoved(WorkspaceMoved),
    WorkspaceReordered(WorkspaceReordered),
    WorkspaceFocused(WorkspaceFocused),
    WorktreeCreated(WorktreeCreated),
    WorktreeOpened(WorktreeOpened),
    WorktreeRemoved(WorktreeRemoved),
    TabCreated(TabCreated),
    TabClosed(TabClosed),
    TabRenamed(TabRenamed),
    TabMoved(TabMoved),
    TabFocused(TabFocused),
    PaneCreated(PaneCreated),
    PaneClosed(PaneClosed),
    PaneUpdated(PaneUpdated),
    PaneFocused(PaneFocused),
    PaneMoved(PaneMoved),
    PaneOutputChanged(PaneOutputChanged),
    PaneExited(PaneExited),
    PaneAgentDetected(PaneAgentDetected),
    PaneAgentStatusChanged(PaneAgentStatusChanged),
    LayoutUpdated(LayoutUpdated),
    PaneAgentStatusChangedSub(PaneAgentStatusChanged),
    PaneOutputMatched(PaneOutputMatchedEvent),
    PaneScrollChanged(PaneScrollChangedEvent),
}

impl HerdrEventData {
    /// Deserialize the payload `data` for the given `kind`.
    pub fn from_kind(kind: HerdrEventKind, data: Value) -> Result<Self> {
        use serde_json::from_value;
        use HerdrEventKind::*;
        Ok(match kind {
            WorkspaceCreated => HerdrEventData::WorkspaceCreated(from_value(data)?),
            WorkspaceUpdated => HerdrEventData::WorkspaceUpdated(from_value(data)?),
            WorkspaceMetadataUpdated => HerdrEventData::WorkspaceMetadataUpdated(from_value(data)?),
            WorkspaceClosed => HerdrEventData::WorkspaceClosed(from_value(data)?),
            WorkspaceRenamed => HerdrEventData::WorkspaceRenamed(from_value(data)?),
            WorkspaceMoved => HerdrEventData::WorkspaceMoved(from_value(data)?),
            WorkspaceReordered => HerdrEventData::WorkspaceReordered(from_value(data)?),
            WorkspaceFocused => HerdrEventData::WorkspaceFocused(from_value(data)?),
            WorktreeCreated => HerdrEventData::WorktreeCreated(from_value(data)?),
            WorktreeOpened => HerdrEventData::WorktreeOpened(from_value(data)?),
            WorktreeRemoved => HerdrEventData::WorktreeRemoved(from_value(data)?),
            TabCreated => HerdrEventData::TabCreated(from_value(data)?),
            TabClosed => HerdrEventData::TabClosed(from_value(data)?),
            TabRenamed => HerdrEventData::TabRenamed(from_value(data)?),
            TabMoved => HerdrEventData::TabMoved(from_value(data)?),
            TabFocused => HerdrEventData::TabFocused(from_value(data)?),
            PaneCreated => HerdrEventData::PaneCreated(from_value(data)?),
            PaneClosed => HerdrEventData::PaneClosed(from_value(data)?),
            PaneUpdated => HerdrEventData::PaneUpdated(from_value(data)?),
            PaneFocused => HerdrEventData::PaneFocused(from_value(data)?),
            PaneMoved => HerdrEventData::PaneMoved(from_value(data)?),
            PaneOutputChanged => HerdrEventData::PaneOutputChanged(from_value(data)?),
            PaneExited => HerdrEventData::PaneExited(from_value(data)?),
            PaneAgentDetected => HerdrEventData::PaneAgentDetected(from_value(data)?),
            PaneAgentStatusChanged => HerdrEventData::PaneAgentStatusChanged(from_value(data)?),
            LayoutUpdated => HerdrEventData::LayoutUpdated(from_value(data)?),
            PaneAgentStatusChangedSub => {
                HerdrEventData::PaneAgentStatusChangedSub(from_value(data)?)
            }
            PaneOutputMatched => HerdrEventData::PaneOutputMatched(from_value(data)?),
            PaneScrollChanged => HerdrEventData::PaneScrollChanged(from_value(data)?),
        })
    }
}

/// Blocking JSON-RPC client over a unix socket.
///
/// IMPORTANT: herdr's socket protocol is one request per connection — the server
/// responds and closes (except for `events.subscribe`, which keeps the stream
/// open). `Client` therefore reconnects for every `call`; `EventStream` holds the
/// long-lived subscription socket.
pub struct Client {
    socket_path: String,
    next_id: u64,
}

pub struct EventStream {
    reader: BufReader<UnixStream>,
}

impl Client {
    /// Connect to the herdr server socket. Resolves the path from
    /// `HERDR_SOCKET_PATH` or the default `~/.config/herdr/herdr.sock`.
    pub fn connect(socket_path: Option<&str>) -> Result<Self> {
        let path = socket_path
            .map(|p| p.to_string())
            .or_else(|| std::env::var("HERDR_SOCKET_PATH").ok())
            .unwrap_or_else(|| default_socket_path().to_string_lossy().into_owned());
        // verify the socket is reachable now so callers fail fast
        let _probe = UnixStream::connect(&path)
            .with_context(|| format!("cannot connect to herdr socket at {path}"))?;
        Ok(Client {
            socket_path: path,
            next_id: 1,
        })
    }

    fn open(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.socket_path)
            .with_context(|| format!("cannot connect to herdr socket at {}", self.socket_path))
    }

    /// Send one request on a fresh connection and wait for its response.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.call_coded(method, params)
            .map_err(|e| anyhow!("herdr error {}: {}", e.code, e.message))
    }

    /// Like `call`, but returns a typed `HerdrError` carrying the protocol
    /// error code — callers can classify (e.g. `agent_prompt_stalled`,
    /// `timeout`) without parsing message strings.
    pub fn call_coded(
        &mut self,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, HerdrError> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let req = json!({ "id": id, "method": method, "params": params });
        let mut line = serde_json::to_string(&req).map_err(|e| HerdrError {
            code: "serialize".into(),
            message: e.to_string(),
        })?;
        line.push('\n');

        let mut reader = BufReader::new(self.open().map_err(|e| HerdrError {
            code: "connect".into(),
            message: e.to_string(),
        })?);
        reader
            .get_mut()
            .write_all(line.as_bytes())
            .map_err(|e| HerdrError {
                code: "io".into(),
                message: format!("write to herdr socket: {e}"),
            })?;
        reader.get_mut().flush().map_err(|e| HerdrError {
            code: "io".into(),
            message: format!("flush herdr socket: {e}"),
        })?;

        loop {
            let mut buf = String::new();
            let n = reader.read_line(&mut buf).map_err(|e| HerdrError {
                code: "io".into(),
                message: format!("read from herdr socket: {e}"),
            })?;
            if n == 0 {
                return Err(HerdrError {
                    code: "io".into(),
                    message: "herdr socket closed while waiting for response".into(),
                });
            }
            let val: Value = serde_json::from_str(buf.trim()).map_err(|e| HerdrError {
                code: "malformed".into(),
                message: format!("malformed herdr response: {buf}: {e}"),
            })?;
            // ignore pushed notifications (no matching id) and other ids
            if val.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                continue;
            }
            if let Some(err) = val.get("error") {
                let code = err
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_string();
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string();
                return Err(HerdrError { code, message });
            }
            return val.get("result").cloned().ok_or_else(|| HerdrError {
                code: "malformed".into(),
                message: format!("herdr response missing result: {val}"),
            });
        }
    }

    /// Subscribe to events; returns a long-lived stream of pushed notifications.
    /// The subscription socket must not be used for anything else.
    pub fn subscribe(&mut self, subscriptions: &[Subscription]) -> Result<EventStream> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let req = json!({
            "id": id,
            "method": "events.subscribe",
            "params": {
                "subscriptions": subscriptions.iter().map(|s| s.to_value()).collect::<Vec<_>>()
            }
        });
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');

        let mut reader = BufReader::new(self.open()?);
        reader
            .get_mut()
            .write_all(line.as_bytes())
            .context("write subscription")?;
        reader.get_mut().flush().context("flush subscription")?;

        // wait for the subscription ack (matching id), skipping earlier pushes
        loop {
            let mut buf = String::new();
            let n = reader
                .read_line(&mut buf)
                .context("read subscription ack")?;
            if n == 0 {
                bail!("herdr socket closed before subscription ack");
            }
            let val: Value = serde_json::from_str(buf.trim())
                .with_context(|| format!("malformed herdr response: {buf}"))?;
            if val.get("id").and_then(Value::as_str) == Some(id.as_str()) {
                if let Some(err) = val.get("error") {
                    bail!(
                        "herdr error {}: {}",
                        err.get("code").and_then(Value::as_str).unwrap_or("error"),
                        err.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    );
                }
                break;
            }
        }
        Ok(EventStream { reader })
    }
}

impl EventStream {
    /// Read one pushed event. `Ok(None)` when the server closes the stream.
    pub fn read_event(&mut self) -> Result<Option<PushEvent>> {
        loop {
            let mut buf = String::new();
            let n = self.reader.read_line(&mut buf)?;
            if n == 0 {
                return Ok(None);
            }
            let val: Value = serde_json::from_str(buf.trim())
                .with_context(|| format!("malformed herdr event: {buf}"))?;
            // skip anything that isn't a push event
            if val.get("event").is_none() {
                continue;
            }
            return Ok(Some(serde_json::from_value(val)?));
        }
    }

    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.reader.get_mut().set_read_timeout(timeout)?;
        Ok(())
    }
}

impl Client {
    // ---- typed helpers ----

    pub fn ping(&mut self) -> Result<Value> {
        self.call("ping", json!({}))
    }

    pub fn workspace_create(
        &mut self,
        cwd: Option<&str>,
        label: Option<&str>,
        focus: bool,
    ) -> Result<(WorkspaceInfo, PaneInfo)> {
        let res = self.call(
            "workspace.create",
            json!({ "cwd": cwd, "label": label, "focus": focus }),
        )?;
        let ws: WorkspaceInfo =
            serde_json::from_value(res.get("workspace").cloned().unwrap_or(res.clone()))
                .context("parse workspace.create result")?;
        let root: PaneInfo =
            serde_json::from_value(res.get("root_pane").cloned().unwrap_or(res.clone()))
                .context("parse workspace.create root_pane")?;
        Ok((ws, root))
    }

    pub fn workspace_list(&mut self) -> Result<Vec<WorkspaceInfo>> {
        let res = self.call("workspace.list", json!({}))?;
        let ws: Vec<WorkspaceInfo> =
            serde_json::from_value(res.get("workspaces").cloned().unwrap_or(res))
                .context("parse workspace.list result")?;
        Ok(ws)
    }

    pub fn workspace_close(&mut self, workspace_id: &str) -> Result<Value> {
        self.call("workspace.close", json!({ "workspace_id": workspace_id }))
    }

    pub fn pane_split(
        &mut self,
        pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<PaneInfo> {
        let res = self.call(
            "pane.split",
            json!({ "target_pane_id": pane_id, "direction": direction, "cwd": cwd }),
        )?;
        serde_json::from_value(res.get("pane").cloned().unwrap_or(res))
            .context("parse pane.split result")
    }

    pub fn pane_zoom(&mut self, pane_id: &str, mode: &str) -> Result<Value> {
        self.call("pane.zoom", json!({ "pane_id": pane_id, "mode": mode }))
    }

    pub fn pane_list(&mut self, workspace_id: Option<&str>) -> Result<Vec<PaneInfo>> {
        let res = self.call("pane.list", json!({ "workspace_id": workspace_id }))?;
        serde_json::from_value(res.get("panes").cloned().unwrap_or(res))
            .context("parse pane.list result")
    }

    pub fn agent_list(&mut self) -> Result<Vec<AgentInfo>> {
        let res = self.call("agent.list", json!({}))?;
        serde_json::from_value(res.get("agents").cloned().unwrap_or(res))
            .context("parse agent.list result")
    }

    pub fn agent_start(
        &mut self,
        name: &str,
        kind: &str,
        pane_id: &str,
        timeout_ms: u64,
        args: &[String],
    ) -> Result<AgentInfo> {
        let res = self.call(
            "agent.start",
            json!({
                "name": name, "kind": kind, "pane_id": pane_id,
                "timeout_ms": timeout_ms, "args": args,
            }),
        )?;
        serde_json::from_value(res.get("agent").cloned().unwrap_or(res))
            .context("parse agent.start result")
    }

    /// Rename an agent record (pi agents often register NAMELESS — the spawn
    /// path applies the requested name via this method, bd-w17).
    pub fn agent_rename(&mut self, target: &str, name: &str) -> Result<Value> {
        self.call("agent.rename", json!({ "target": target, "name": name }))
    }

    pub fn agent_prompt(
        &mut self,
        target: &str,
        text: &str,
        wait: Option<WaitOptions>,
    ) -> Result<AgentInfo> {
        self.agent_prompt_coded(target, text, wait)
            .map_err(|e| anyhow!("herdr error {}: {}", e.code, e.message))
    }

    /// `agent.prompt` returning a typed `HerdrError` so callers can classify
    /// protocol error codes (e.g. `agent_prompt_stalled`, `timeout`).
    pub fn agent_prompt_coded(
        &mut self,
        target: &str,
        text: &str,
        wait: Option<WaitOptions>,
    ) -> std::result::Result<AgentInfo, HerdrError> {
        let wait = wait.map(|w| {
            json!({ "timeout_ms": w.timeout_ms, "until": w.until.iter().map(|s| s.as_str()).collect::<Vec<_>>() })
        });
        let res = self.call_coded(
            "agent.prompt",
            json!({ "target": target, "text": text, "wait": wait }),
        )?;
        serde_json::from_value(res.get("agent").cloned().unwrap_or(res)).map_err(|e| HerdrError {
            code: "parse".into(),
            message: e.to_string(),
        })
    }

    pub fn agent_wait(
        &mut self,
        target: &str,
        until: &[AgentStatus],
        timeout_ms: Option<u64>,
    ) -> Result<AgentInfo> {
        let res = self.call(
            "agent.wait",
            json!({
                "target": target,
                "until": until.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                "timeout_ms": timeout_ms,
            }),
        )?;
        serde_json::from_value(res.get("agent").cloned().unwrap_or(res))
            .context("parse agent.wait result")
    }

    pub fn agent_read(&mut self, target: &str, source: &str, lines: Option<u32>) -> Result<String> {
        let res = self.call(
            "agent.read",
            json!({ "target": target, "source": source, "lines": lines, "format": "text", "strip_ansi": true }),
        )?;
        let text = res
            .get("content")
            .or_else(|| res.get("text"))
            .or_else(|| res.get("output"))
            .or_else(|| res.get("read").and_then(|r| r.get("text")))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("agent.read result has no content: {res}"))?;
        Ok(text.to_string())
    }

    /// Block on `pane.wait_for_output` — wait until `pattern` appears in the
    /// pane's terminal output.
    ///
    /// Docs' caveat (verbatim): "`pane wait-output` does not interpret agent
    /// lifecycle. It polls the selected terminal snapshot and searches it
    /// immediately, so output that already exists can match." This is a TEXT
    /// wait — for agent state use [`Client::agent_wait`]; the two are
    /// complementary and pane waits deliberately do NOT understand lifecycle.
    ///
    /// `mode` is `"match"` (literal substring) or `"regex"` (Rust regex,
    /// herdr-side), mirroring the CLI's `--match`/`--regex`. The wait blocks
    /// the socket — one request per connection, same as `agent_wait`.
    ///
    /// The snapshot (`read`) is raw terminal text (strip-ansi applied
    /// server-side); it can be large — treat it as an opaque string.
    ///
    /// Returns a typed [`HerdrError`] so callers can classify: a non-matching
    /// pattern with a short timeout errors with code `"timeout"` (it does NOT
    /// hang), other protocol failures carry their own codes.
    pub fn pane_wait_output(
        &mut self,
        pane_id: &str,
        mode: &str,
        pattern: &str,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<PaneWaitResult, HerdrError> {
        let params = wait_for_output_params(pane_id, mode, pattern, timeout_ms)?;
        let res = self.call_coded("pane.wait_for_output", params)?;
        parse_wait_for_output_result(&res)
    }

    pub fn agent_send_keys(&mut self, target: &str, keys: &[&str]) -> Result<Value> {
        self.call("agent.send_keys", json!({ "target": target, "keys": keys }))
    }

    pub fn worktree_list(&mut self, workspace_id: Option<&str>) -> Result<Vec<WorktreeInfo>> {
        let res = self.call("worktree.list", json!({ "workspace_id": workspace_id }))?;
        serde_json::from_value(res.get("worktrees").cloned().unwrap_or(res))
            .context("parse worktree.list result")
    }

    pub fn worktree_create(
        &mut self,
        workspace_id: &str,
        branch: &str,
        base: Option<&str>,
        label: Option<&str>,
        focus: bool,
    ) -> Result<Value> {
        self.call(
            "worktree.create",
            json!({
                "workspace_id": workspace_id, "branch": branch, "base": base,
                "label": label, "focus": focus,
            }),
        )
    }

    pub fn worktree_remove(&mut self, workspace_id: &str, force: bool) -> Result<Value> {
        self.call(
            "worktree.remove",
            json!({ "workspace_id": workspace_id, "force": force }),
        )
    }
}

#[derive(Debug, Clone)]
pub struct WaitOptions {
    pub timeout_ms: Option<u64>,
    pub until: Vec<AgentStatus>,
}

/// Typed herdr protocol error carrying the machine-readable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "herdr error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for HerdrError {}

/// Classification of a confirmed `agent.prompt` outcome (see `classify_send_result`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendVerdict {
    /// Prompt landed: herdr accepted it and the agent reached a working/blocked state.
    Landed(Option<AgentStatus>),
    /// herdr's lifecycle-change rule fired (`agent_prompt_stalled`): no landing observed.
    Stalled,
    /// Caller-side wait expired (`timeout`): the prompt may have started — ambiguous.
    TimedOut,
    /// Any other herdr error code (e.g. `agent_not_running`).
    Coded(String),
}

/// Map a herdr error code + observed agent status to a send verdict.
///
/// Classifies on the protocol error CODE, never by parsing message strings
/// (messages are human text and may change between server versions).
pub fn classify_send_result(code: Option<&str>, agent_status: Option<AgentStatus>) -> SendVerdict {
    match code {
        Some("agent_prompt_stalled") => SendVerdict::Stalled,
        Some("timeout") => SendVerdict::TimedOut,
        Some(other) => SendVerdict::Coded(other.to_string()),
        None => SendVerdict::Landed(agent_status),
    }
}

/// Result of a `pane.wait_for_output` wait.
///
/// `read` is the matched terminal snapshot's text, treated as an opaque
/// string — pane waits do NOT interpret agent lifecycle (see
/// [`Client::pane_wait_output`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneWaitResult {
    pub pane_id: String,
    /// The line the pattern matched on, if the server reports one.
    pub matched_line: Option<String>,
    /// Snapshot text at match time (may be large; do not post-process).
    pub read: String,
}

/// Request params for `pane.wait_for_output` (protocol v18).
///
/// `mode` mirrors the CLI's `--match`/`--regex`: `"match"` is a literal
/// substring search, `"regex"` a Rust regex (herdr-side). Anything else is a
/// caller bug — bail with code `"usage"`.
pub fn wait_for_output_params(
    pane_id: &str,
    mode: &str,
    pattern: &str,
    timeout_ms: Option<u64>,
) -> std::result::Result<Value, HerdrError> {
    let match_type = match mode {
        "match" => "substring",
        "regex" => "regex",
        other => {
            return Err(HerdrError {
                code: "usage".into(),
                message: format!(
                    "unknown pane wait-output mode: {other} (expected \"match\" or \"regex\")"
                ),
            })
        }
    };
    Ok(json!({
        "pane_id": pane_id,
        "source": "recent",
        "match": { "type": match_type, "value": pattern },
        "strip_ansi": true,
        "timeout_ms": timeout_ms,
    }))
}

/// Parse a `pane.wait_for_output` result envelope. Defensive: `read` is an
/// object carrying the snapshot text; any shape drift becomes code `"parse"`.
pub fn parse_wait_for_output_result(
    res: &Value,
) -> std::result::Result<PaneWaitResult, HerdrError> {
    let parse_err = |msg: String| HerdrError {
        code: "parse".into(),
        message: msg,
    };
    if res.get("type").and_then(Value::as_str) != Some("output_matched") {
        return Err(parse_err(format!(
            "unexpected pane.wait_for_output result: {res}"
        )));
    }
    let pane_id = res
        .get("pane_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            parse_err(format!(
                "pane.wait_for_output result missing pane_id: {res}"
            ))
        })?
        .to_string();
    let matched_line = res
        .get("matched_line")
        .and_then(Value::as_str)
        .map(str::to_string);
    let read = res
        .get("read")
        .and_then(|r| r.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            parse_err(format!(
                "pane.wait_for_output result missing read.text: {res}"
            ))
        })?
        .to_string();
    Ok(PaneWaitResult {
        pane_id,
        matched_line,
        read,
    })
}

/// Subscription event kinds accepted by `events.subscribe` (dotted wire
/// names). Distinct from `HerdrEventKind`'s server-event namespace: these
/// are the *request* filters, and the server echoes the same name on pushes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // PaneOutputMatched/PaneScrollChanged await the Phase-2 reactor
#[allow(clippy::enum_variant_names)] // wire-fidelity names
pub enum SubscriptionEventKind {
    PaneAgentStatusChanged,
    PaneOutputMatched,
    PaneScrollChanged,
}

impl SubscriptionEventKind {
    /// The wire name for this subscription event (dotted).
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            SubscriptionEventKind::PaneAgentStatusChanged => "pane.agent_status_changed",
            SubscriptionEventKind::PaneOutputMatched => "pane.output_matched",
            SubscriptionEventKind::PaneScrollChanged => "pane.scroll_changed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub event: SubscriptionEventKind,
    /// Required by the live server: `pane.agent_status_changed` (and the
    /// other pane.* subscriptions) reject requests without a concrete
    /// `pane_id` (verified against live herdr v19 — a `null` pane_id fails
    /// with "missing field `pane_id`" and the connection is closed).
    pub pane_id: String,
    pub agent_status: Option<AgentStatus>,
}

impl Subscription {
    /// Subscribe to agent status changes for a specific pane. To watch a
    /// whole workspace, build one subscription per pane (the server accepts
    /// a `subscriptions` array in a single request).
    pub fn agent_status_changed(pane_id: String) -> Self {
        Subscription {
            event: SubscriptionEventKind::PaneAgentStatusChanged,
            pane_id,
            agent_status: None,
        }
    }

    /// Narrow the subscription to a single status (used by tests; the
    /// Phase-2 reactor filters agent-status pushes this way).
    #[allow(dead_code)]
    pub fn with_agent_status(mut self, status: AgentStatus) -> Self {
        self.agent_status = Some(status);
        self
    }

    pub fn to_value(&self) -> Value {
        json!({
            "type": self.event.as_wire_str(),
            "pane_id": self.pane_id,
            "agent_status": self.agent_status.map(|s| s.as_str()),
        })
    }
}

/// Default socket path used when HERDR_SOCKET_PATH and config are absent.
pub fn default_socket_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| Path::new("~").to_path_buf())
        .join("herdr/herdr.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_status_roundtrip() {
        assert_eq!(AgentStatus::parse("blocked").unwrap(), AgentStatus::Blocked);
        assert_eq!(AgentStatus::Blocked.as_str(), "blocked");
        assert!(AgentStatus::parse("bogus").is_err());
    }

    /// Every wire name round-trips: parse(as_wire_str(k)) == k, across both
    /// namespaces (26 snake server events + 3 dotted subscription events).
    #[test]
    fn kind_wire_roundtrip() {
        let kinds = [
            "workspace_created",
            "workspace_updated",
            "workspace_metadata_updated",
            "workspace_closed",
            "workspace_renamed",
            "workspace_moved",
            "workspace_reordered",
            "workspace_focused",
            "worktree_created",
            "worktree_opened",
            "worktree_removed",
            "tab_created",
            "tab_closed",
            "tab_renamed",
            "tab_moved",
            "tab_focused",
            "pane_created",
            "pane_closed",
            "pane_updated",
            "pane_focused",
            "pane_moved",
            "pane_output_changed",
            "pane_exited",
            "pane_agent_detected",
            "pane_agent_status_changed",
            "layout_updated",
            "pane.agent_status_changed",
            "pane.output_matched",
            "pane.scroll_changed",
        ];
        assert_eq!(kinds.len(), 29, "26 server + 3 subscription kinds");
        for wire in kinds {
            let k = HerdrEventKind::parse(wire).unwrap_or_else(|e| panic!("{wire}: {e}"));
            assert_eq!(k.as_wire_str(), wire, "as_wire_str round-trips {wire}");
        }
        assert!(HerdrEventKind::parse("not_an_event").is_err());
    }

    /// The typed Subscription builder emits the exact wire request: the live
    /// server requires a concrete `pane_id` (no nulls, no workspace_id).
    #[test]
    fn subscription_to_value_wire_shape() {
        let v = Subscription::agent_status_changed("w1:p2".into())
            .with_agent_status(AgentStatus::Blocked)
            .to_value();
        assert_eq!(v["type"], "pane.agent_status_changed");
        assert_eq!(v["pane_id"], "w1:p2");
        assert_eq!(v["agent_status"], "blocked");
        assert!(
            v.get("workspace_id").is_none(),
            "workspace_id is not part of the wire contract"
        );
    }

    #[test]
    fn parse_agent_info() {
        let v = json!({
            "agent": "opencode",
            "agent_status": "working",
            "cwd": "/proj",
            "name": "flywheel-x-oc",
            "pane_id": "w1:p2",
            "tab_id": "w1:t1",
            "workspace_id": "w1",
            "terminal_id": "t1",
            "focused": false,
            "interactive_ready": true,
            "revision": 3
        });
        let a: AgentInfo = serde_json::from_value(v).unwrap();
        assert_eq!(a.agent_status, AgentStatus::Working);
        assert_eq!(a.pane_id, "w1:p2");
        assert!(a.interactive_ready);
    }

    #[test]
    fn parse_push_event() {
        let v = json!({
            "event": "pane.agent_status_changed",
            "data": {
                "pane_id": "w1:p2",
                "workspace_id": "w1",
                "agent_status": "blocked",
                "agent": "opencode"
            }
        });
        let e: PushEvent = serde_json::from_value(v).unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneAgentStatusChangedSub);
        match e.data {
            HerdrEventData::PaneAgentStatusChangedSub(s) => {
                assert_eq!(s.agent_status, AgentStatus::Blocked);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// Same coverage as the parse tests above but with explicit per-file
    /// includes (include_str! requires literal paths). One parse per event
    /// kind — 26 server events + 3 subscription events = 29 fixtures, all
    /// schema-derived (see `tests/fixtures/herdr-events/`).
    #[test]
    fn all_event_fixtures_parse_typed() {
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/workspace_created.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorkspaceCreated);
        match e.data {
            HerdrEventData::WorkspaceCreated(_) => {}
            other => panic!("unexpected variant: {other:?}"),
        }

        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/workspace_updated.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorkspaceUpdated);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/workspace_metadata_updated.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorkspaceMetadataUpdated);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/workspace_closed.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorkspaceClosed);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/workspace_renamed.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorkspaceRenamed);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/workspace_moved.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorkspaceMoved);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/workspace_reordered.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorkspaceReordered);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/workspace_focused.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorkspaceFocused);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/worktree_created.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorktreeCreated);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/worktree_opened.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorktreeOpened);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/worktree_removed.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::WorktreeRemoved);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/tab_created.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::TabCreated);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/tab_closed.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::TabClosed);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/tab_renamed.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::TabRenamed);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/tab_moved.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::TabMoved);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/tab_focused.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::TabFocused);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_created.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneCreated);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_closed.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneClosed);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_updated.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneUpdated);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_focused.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneFocused);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_moved.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneMoved);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_output_changed.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneOutputChanged);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_exited.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneExited);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_agent_detected.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneAgentDetected);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_agent_status_changed.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneAgentStatusChanged);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/layout_updated.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::LayoutUpdated);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_agent_status_changed_sub.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneAgentStatusChangedSub);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_output_matched.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneOutputMatched);
        let e: PushEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr-events/pane_scroll_changed.json"
        ))
        .unwrap();
        assert_eq!(e.kind, HerdrEventKind::PaneScrollChanged);
    }

    #[test]
    fn request_framing_is_single_line_json() {
        let req = json!({ "id": "1", "method": "ping", "params": {} });
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        assert_eq!(line.matches('\n').count(), 1);
        let parsed: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["method"], "ping");
    }

    #[test]
    fn wait_options_serialize() {
        let w = WaitOptions {
            timeout_ms: Some(30_000),
            until: vec![AgentStatus::Idle, AgentStatus::Blocked],
        };
        let v = json!({ "timeout_ms": w.timeout_ms, "until": w.until.iter().map(|s| s.as_str()).collect::<Vec<_>>() });
        assert_eq!(v["until"][0], "idle");
        assert_eq!(v["timeout_ms"], 30_000);
    }

    #[test]
    fn landing_wait_options_serialize() {
        // `__send --confirm` waits for working|blocked — landing semantics,
        // NOT idle/done (a long task must not hold the leaf for its duration).
        let until = vec![AgentStatus::Working.as_str(), AgentStatus::Blocked.as_str()];
        let v = json!({ "timeout_ms": 45_000, "until": until });
        assert_eq!(v["until"][0], "working");
        assert_eq!(v["until"][1], "blocked");
        assert_eq!(v["timeout_ms"], 45_000);
    }

    #[test]
    fn classify_send_result_verdicts() {
        // stalled: herdr's 5s lifecycle-change rule fired
        assert_eq!(
            classify_send_result(Some("agent_prompt_stalled"), None),
            SendVerdict::Stalled
        );
        // timeout: caller-side wait expired — prompt MAY have started
        assert_eq!(
            classify_send_result(Some("timeout"), Some(AgentStatus::Working)),
            SendVerdict::TimedOut
        );
        // other codes classify as coded error (no message-string parsing)
        assert_eq!(
            classify_send_result(Some("agent_not_running"), None),
            SendVerdict::Coded("agent_not_running".to_string())
        );
        // success: prompt landed, agent status observed
        assert_eq!(
            classify_send_result(None, Some(AgentStatus::Working)),
            SendVerdict::Landed(Some(AgentStatus::Working))
        );
        assert_eq!(classify_send_result(None, None), SendVerdict::Landed(None));
    }

    // ---- pane.wait_for_output (bd-pane-wait-output-helper) ----

    #[test]
    fn wait_for_output_params_shape() {
        // substring mode + timeout present
        let v = wait_for_output_params("w13:p1", "match", "listening", Some(30_000)).unwrap();
        assert_eq!(v["pane_id"], "w13:p1");
        assert_eq!(v["source"], "recent");
        assert_eq!(v["match"]["type"], "substring");
        assert_eq!(v["match"]["value"], "listening");
        assert_eq!(v["strip_ansi"], true);
        assert_eq!(v["timeout_ms"], 30_000);
        // regex mode + timeout absent (serializes as null, schema-valid)
        let v = wait_for_output_params("w13:p1", "regex", r"ready\.\.\.", None).unwrap();
        assert_eq!(v["match"]["type"], "regex");
        assert_eq!(v["match"]["value"], r"ready\.\.\.");
        assert!(v["timeout_ms"].is_null());
    }

    #[test]
    fn wait_for_output_params_rejects_unknown_mode() {
        assert!(wait_for_output_params("w13:p1", "fuzzy", "x", None).is_err());
    }

    #[test]
    fn parse_wait_for_output_result_happy_path() {
        // captured from the live server (protocol v18)
        let v = json!({
            "type": "output_matched",
            "pane_id": "w13:p1",
            "revision": 0,
            "matched_line": "❯ sleep 1; echo probe-done-123",
            "read": {
                "pane_id": "w13:p1", "workspace_id": "w13", "tab_id": "w13:t1",
                "source": "recent_unwrapped", "format": "text",
                "text": "\n❯ sleep 1; echo probe-done-123\nprobe-done-123\n",
                "revision": 0, "truncated": false
            }
        });
        let r = parse_wait_for_output_result(&v).unwrap();
        assert_eq!(r.pane_id, "w13:p1");
        assert_eq!(
            r.matched_line.as_deref(),
            Some("❯ sleep 1; echo probe-done-123")
        );
        assert!(r.read.contains("probe-done-123"));
    }

    #[test]
    fn parse_wait_for_output_result_missing_matched_line() {
        // a match with no matched_line still yields a result (defensive)
        let v = json!({
            "type": "output_matched",
            "pane_id": "w13:p1",
            "read": { "text": "done" }
        });
        let r = parse_wait_for_output_result(&v).unwrap();
        assert_eq!(r.pane_id, "w13:p1");
        assert_eq!(r.matched_line, None);
        assert_eq!(r.read, "done");
    }

    #[test]
    fn parse_wait_for_output_result_missing_read_text() {
        // read is an object with a text field; anything else is a parse error
        let v = json!({ "type": "output_matched", "pane_id": "w13:p1", "read": {} });
        let e = parse_wait_for_output_result(&v).unwrap_err();
        assert_eq!(e.code, "parse");
    }

    #[test]
    fn parse_wait_for_output_result_rejects_unknown_type() {
        let v = json!({ "type": "something_else", "pane_id": "w13:p1", "read": {} });
        let e = parse_wait_for_output_result(&v).unwrap_err();
        assert_eq!(e.code, "parse");
    }

    /// Wire verification against a live herdr server. Uses env `PANE_ID` or the
    /// first pane from `pane_list`. Soft-returns if herdr is down.
    #[test]
    fn pane_wait_output_live() {
        let mut c = match Client::connect(None) {
            Ok(c) => c,
            Err(_) => return,
        };
        let pane_id = match std::env::var("PANE_ID") {
            Ok(p) => p,
            Err(_) => {
                let panes = c.pane_list(None).unwrap_or_default();
                match panes.first() {
                    Some(p) => p.pane_id.clone(),
                    None => return,
                }
            }
        };
        // non-matching pattern must ERROR (code "timeout"), not hang
        let e = c
            .pane_wait_output(
                &pane_id,
                "match",
                "THIS-WILL-NEVER-MATCH-xyzzy-flywheel-e2e",
                Some(1_500),
            )
            .unwrap_err();
        assert_eq!(e.code, "timeout", "expected timeout code, got {e:?}");
        // matching common text is optional (pane may be empty)
        let _ = c.pane_wait_output(&pane_id, "match", "e", Some(1_500));
    }

    /// Live protocol round-trip against a running herdr server.
    #[test]
    fn live_snapshot_roundtrip() {
        let mut c = match Client::connect(None) {
            Ok(c) => c,
            Err(_) => return,
        };
        let workspaces = c.workspace_list().unwrap();
        let panes = c.pane_list(None).unwrap();
        let agents = c.agent_list().unwrap();
        // parse path succeeded even if empty; live sessions usually have data
        if workspaces.is_empty() {
            return;
        }
        let first = workspaces.first().unwrap();
        let _ = c.worktree_list(Some(&first.workspace_id)).unwrap();
        let _ = panes;
        let _ = agents;
    }

    /// Live typed event subscribe: if a free pane exists, start a short agent
    /// and assert at least one push parses. Soft-return if herdr/pane missing.
    #[test]
    fn live_events_subscribe_typed() {
        let mut c = match Client::connect(None) {
            Ok(c) => c,
            Err(_) => return,
        };
        let panes = c.pane_list(None).unwrap_or_default();
        let occupied: Vec<String> = c
            .agent_list()
            .unwrap_or_default()
            .iter()
            .map(|a| a.pane_id.clone())
            .collect();
        let Some(pane) = panes.iter().find(|p| !occupied.contains(&p.pane_id)) else {
            // no free pane — still prove subscribe+timeout works on an occupied pane
            if let Some(p) = panes.first() {
                let mut stream = c
                    .subscribe(&[Subscription::agent_status_changed(p.pane_id.clone())])
                    .unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(400)))
                    .unwrap();
                let _ = stream.read_event(); // timeout or event — both ok
            }
            return;
        };
        let name = format!("live-probe-{}", std::process::id());
        let mut stream = c
            .subscribe(&[Subscription::agent_status_changed(pane.pane_id.clone())])
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(5_000)))
            .unwrap();

        // drive the transition on a separate client (the subscription socket
        // must stay request-free)
        let mut c2 = Client::connect(None).unwrap();
        // agent_start may fail if kind missing — then just assert stream timeout path
        if c2
            .agent_start(&name, "opencode", &pane.pane_id, 20_000, &[])
            .is_err()
        {
            let _ = stream.read_event();
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
        let _ = c2.agent_prompt(&name, "Reply with the single word: done.", None);

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut seen = 0;
        loop {
            if std::time::Instant::now() >= deadline {
                break;
            }
            match stream.read_event() {
                Ok(Some(ev)) => {
                    // typed: the payload must be the agent-status variant with
                    // parsed fields — a hand-parse (`Value`) would hide schema
                    // drift here
                    match &ev.data {
                        HerdrEventData::PaneAgentStatusChangedSub(s)
                        | HerdrEventData::PaneAgentStatusChanged(s) => {
                            assert_eq!(s.pane_id, pane.pane_id);
                            assert!(!s.workspace_id.is_empty());
                            seen += 1;
                        }
                        other => panic!("unexpected typed variant: {other:?}"),
                    }
                }
                Ok(None) => {
                    // server closed the stream (e.g. the pane died) — fine
                    // after we've captured at least one push
                    if seen >= 1 {
                        break;
                    }
                    panic!("stream closed before any push");
                }
                Err(_) => continue, // transient read error: keep polling
            }
            if seen >= 3 {
                break;
            }
        }
        assert!(
            seen >= 1,
            "expected at least one status-change push for {name}"
        );
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn parse_protocol_reads_top_level_field() {
        let raw = r#"{"protocol": 19, "schema_version": 1, "schemas": {}}"#;
        assert_eq!(parse_protocol(raw), Some(19));
    }

    #[test]
    fn parse_protocol_reads_v19_from_schema_fixture() {
        // Trimmed from the live `herdr api schema --json` output (protocol 19,
        // herdr 0.8.0-preview.2026-08-04-d78e3d3b5126) — see tests/fixtures/.
        let raw = include_str!("../tests/fixtures/herdr-schema-v19.json");
        assert_eq!(parse_protocol(raw), Some(19));
    }

    #[test]
    fn parse_protocol_failures_are_none() {
        assert_eq!(parse_protocol("not json"), None);
        assert_eq!(parse_protocol(r#"{"schemas": {}}"#), None);
        assert_eq!(parse_protocol(r#"{"protocol": "18"}"#), None);
        assert_eq!(parse_protocol(""), None);
    }
}

#[cfg(test)]
mod workspace_resolve_tests {
    use super::*;

    fn ws(id: &str, label: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.to_string(),
            number: 1,
            label: label.to_string(),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: format!("{id}:t1"),
            agent_status: AgentStatus::Unknown,
            cwd: None,
        }
    }

    fn agent(name: Option<&str>, ws_id: &str) -> AgentInfo {
        AgentInfo {
            agent: Some("pi".into()),
            agent_session: None,
            agent_status: AgentStatus::Idle,
            cwd: Some("/proj".into()),
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: true,
            launch_pending: false,
            name: name.map(String::from),
            pane_id: "w1:p1".into(),
            revision: 1,
            state_change_seq: 0,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term_1".into(),
            terminal_title: None,
            workspace_id: ws_id.to_string(),
        }
    }

    #[test]
    fn single_match_wins() {
        let matches = vec![ws("w1", "flywheel-x"), ws("w2", "other")];
        let agents = vec![];
        assert_eq!(resolve_workspace_id(&matches, &agents, "flywheel-x-"), "w1");
    }

    #[test]
    fn duplicate_labels_prefer_workspace_with_managed_agents() {
        // w14 = stale (no agents), w15 = live (managed agent) — pick w15
        let matches = vec![ws("w14", "flywheel-x"), ws("w15", "flywheel-x")];
        let agents = vec![agent(Some("flywheel-x-pi"), "w15")];
        assert_eq!(
            resolve_workspace_id(&matches, &agents, "flywheel-x-"),
            "w15"
        );
    }

    #[test]
    fn duplicate_labels_both_dead_fall_back_to_first() {
        let matches = vec![ws("w14", "flywheel-x"), ws("w15", "flywheel-x")];
        let agents = vec![agent(Some("unrelated-oc"), "w15")];
        assert_eq!(
            resolve_workspace_id(&matches, &agents, "flywheel-x-"),
            "w14"
        );
    }

    #[test]
    fn duplicate_labels_both_live_prefer_first_deterministically() {
        let matches = vec![ws("w14", "flywheel-x"), ws("w15", "flywheel-x")];
        let agents = vec![
            agent(Some("flywheel-x-oc"), "w14"),
            agent(Some("flywheel-x-oc2"), "w15"),
        ];
        assert_eq!(
            resolve_workspace_id(&matches, &agents, "flywheel-x-"),
            "w14"
        );
    }

    #[test]
    fn nameless_managed_agent_does_not_count() {
        // a pi record without a name is not a managed-agent signal — the
        // rename fix (bd-w17) names it at spawn time
        let matches = vec![ws("w14", "flywheel-x"), ws("w15", "flywheel-x")];
        let agents = vec![agent(None, "w15")];
        assert_eq!(
            resolve_workspace_id(&matches, &agents, "flywheel-x-"),
            "w14"
        );
    }
}
