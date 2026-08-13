//! flywheel-local herdr shim (ADR-0003 Decision 3A, bd-o8j.2).
//!
//! `src/herdr.rs` was deleted; toron-core owns the SINGLE herdr client
//! definition. This module re-exports the toron-core herdr types and adds the
//! flywheel-specific helpers that main.rs / the backend layer still consume
//! but toron-core does not provide (event-payload decoding, `pane_split`,
//! `pane_zoom`, `pane_wait_output`, `worktree_list`, the subscription stream
//! adapter).
//!
//! The shim is PURE DELEGATION over the toron-core `Client`:
//! - every protocol call goes through `toron_core::backend::herdr::Client`
//!   (`call` / `call_coded` / `subscribe`);
//! - no JSON-RPC / socket / error-classification logic is re-implemented;
//! - the only non-delegation code here is serde shape-parsing of the payloads
//!   flywheel routes on, which toron-core deliberately does not model.
//!
//! `set_read_timeout`: the toron-core `EventStream` exposes no socket handle,
//! so this shim wraps it in a reader thread + bounded channel and implements
//! `set_read_timeout` via `recv_timeout` — behavior-identical to the old
//! socket SO_RCVTIMEO (blocking read returns Err on expiry), no protocol
//! reimplementation.
//!
//! governed-by: ADR-0003

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

pub use toron_core::backend::herdr::{
    classify_send_result, AgentInfo, Client, PaneInfo, SendVerdict, Subscription, WaitOptions,
    WorkspaceInfo, HDR_PROTOCOL,
};
pub use toron_core::backend::types::AgentStatus;

/// Parse `AgentStatus` from its wire string (the old inherent `parse` —
/// toron-core exposes `FromStr` instead, so this is a thin free wrapper).
pub fn parse_agent_status(s: &str) -> Result<AgentStatus> {
    s.parse().map_err(|e| anyhow!("{e}"))
}

/// Parse the protocol version from `herdr api schema --json` output (the
/// top-level `protocol` field). `None` on any failure — the caller decides
/// how to surface an unreadable schema.
pub fn parse_protocol(raw: &str) -> Option<u64> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("protocol").and_then(Value::as_u64))
}

/// Strip terminal control sequences and TUI box-drawing chrome from a pane
/// read (bd-3hd).
///
/// herdr's `strip_ansi` (sent on every `agent.read`) only removes CSI color
/// codes; the `visible` source keeps the full alternate-screen render —
/// opencode's TUI border glyphs (┏━┃┗ etc) plus cursor/hyperlink sequences —
/// so reads of a working agent are still garbled. This runs on ALL read
/// sources (recent, visible, recent-unwrapped) at the flywheel boundary.
///
/// Removed:
/// - CSI sequences: `ESC [` + params/intermediates + final byte (colors,
///   cursor moves, erase, SGR...);
/// - OSC sequences: `ESC ]` ... terminated by `ESC \` (ST) or BEL (hyperlinks,
///   titles, clipboard);
/// - other one-shot escapes: `ESC` + single char (`ESC 7`/`ESC 8` save-restore,
///   `ESC M`, `ESC =`...);
/// - box-drawing + block-element glyphs (U+2500..U+259F) that form TUI borders
///   (─│┃┏┓┗┛┣┫┳┻╋═║╔╗╚╝╠╣╦╩╬ ▀▄█░▒▓). Content is preserved verbatim.
pub fn sanitize_pane_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    loop {
                        match chars.next() {
                            Some('\u{07}') => break,
                            Some('\u{1b}') => {
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                            Some(_) => {}
                            None => break,
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if ('\u{2500}'..='\u{259f}').contains(&c) {
            continue;
        }
        out.push(c);
    }
    out
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

// ---------------------------------------------------------------------------
// flywheel-local wire types (deleted `src/herdr.rs` — verbatim). toron-core
// models the client, not the per-event payload shapes flywheel routes on.
// ---------------------------------------------------------------------------

/// push notification payload wrapper (`kind` = wire event name, `data` =
/// typed payload). Parsed from the raw `(kind, Value)` the toron-core
/// `EventStream::read_event` yields.
#[derive(Debug, Clone)]
#[allow(dead_code)] // bin matches on the typed `data` variant; `kind` is informational
pub struct PushEvent {
    pub kind: HerdrEventKind,
    pub data: HerdrEventData,
}

/// All herdr event kinds: the 26 server events (snake_case wire names) plus
/// the 3 subscription pushes (dot namespaced wire names). `parse` accepts
/// both namespaces; `as_wire_str` returns the exact wire name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // consumed by tests + the reactor; bin matches on data today
pub enum HerdrEventKind {
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
    PaneAgentStatusChangedSub,
    PaneOutputMatched,
    PaneScrollChanged,
}

impl HerdrEventKind {
    /// Parse a wire event name. Accepts both namespaces: the 26 snake_case
    /// server events and the 3 dotted subscription events.
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
}

// ---- typed event payloads ----
//
// Field sets mirror the live schema (`herdr api schema --json` →
// `schemas/event/$defs/EventData.oneOf`). Nested objects that flywheel does
// not consume yet stay `Value` — the goal is type safety for the fields we
// route on, not a full model of every payload. `WorkspaceInfo` and `PaneInfo`
// (re-exported from toron-core) are reused for the nested payloads.

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

/// flywheel-shaped worktree record (`worktree.list`). toron-core's
/// `WorktreeInfo` models the create/remove payloads, not this listing shape.
#[derive(Debug, Clone, Deserialize)]
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
#[allow(dead_code)] // variant payloads consumed by tests + reactor
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

/// Map a toron-core `BackendError` onto the flywheel-local typed error.
/// Classification stays on the machine code (`BackendErrorCode::as_str`).
fn to_herdr_error(e: toron_core::backend::error::BackendError) -> HerdrError {
    HerdrError {
        code: e.code().as_str().to_string(),
        message: e.message.clone(),
    }
}

/// Result of a `pane.wait_for_output` wait.
///
/// `read` is the matched terminal snapshot's text, treated as an opaque
/// string — pane waits do NOT interpret agent lifecycle.
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

// ---------------------------------------------------------------------------
// Pure-delegation client helpers (methods flywheel used that toron-core's
// `Client` does not expose). Each forwards to the toron-core `Client`'s raw
// `call` / `call_coded` — zero protocol logic here.
// ---------------------------------------------------------------------------

/// `pane.split` — delegation over `Client::call`.
pub fn pane_split(
    c: &mut Client,
    pane_id: &str,
    direction: &str,
    cwd: Option<&str>,
) -> Result<PaneInfo> {
    let res = c.call(
        "pane.split",
        json!({ "target_pane_id": pane_id, "direction": direction, "cwd": cwd }),
    )?;
    serde_json::from_value(res.get("pane").cloned().unwrap_or(res))
        .context("parse pane.split result")
}

/// `pane.zoom` — delegation over `Client::call`.
pub fn pane_zoom(c: &mut Client, pane_id: &str, mode: &str) -> Result<Value> {
    c.call("pane.zoom", json!({ "pane_id": pane_id, "mode": mode }))
        .map_err(|e| anyhow!("herdr pane.zoom failed: {e}"))
}

/// `pane.wait_for_output` — delegation over `Client::call_coded`. Returns a
/// typed `HerdrError` so callers can classify (`timeout` code on a miss).
pub fn pane_wait_output(
    c: &mut Client,
    pane_id: &str,
    mode: &str,
    pattern: &str,
    timeout_ms: Option<u64>,
) -> std::result::Result<PaneWaitResult, HerdrError> {
    let params = wait_for_output_params(pane_id, mode, pattern, timeout_ms)?;
    let res = c
        .call_coded("pane.wait_for_output", params)
        .map_err(to_herdr_error)?;
    parse_wait_for_output_result(&res)
}

/// `worktree.list` — delegation over `Client::call`, returning the
/// flywheel-shaped [`WorktreeInfo`].
pub fn worktree_list(c: &mut Client, workspace_id: Option<&str>) -> Result<Vec<WorktreeInfo>> {
    let res = c.call("worktree.list", json!({ "workspace_id": workspace_id }))?;
    serde_json::from_value(res.get("worktrees").cloned().unwrap_or(res))
        .context("parse worktree.list result")
}

// ---------------------------------------------------------------------------
// Process liveness probe (bd-1np): `pane.process_info`.
// ---------------------------------------------------------------------------

/// One foreground process in a pane (`pane.process_info`).
///
/// The agent process (opencode/pi/grok/omp) appears here with `pid ==
/// foreground_process_group_id` while it runs; when the process dies the pane
/// falls back to the shell prompt and the agent binary disappears from the
/// list. This is the ground-truth liveness signal `flywheel recover` probes
/// (a pane can exist in herdr while the process inside it is dead).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneProcessInfoProcess {
    pub pid: u64,
    pub name: String,
    #[serde(default)]
    pub argv0: Option<String>,
}

/// Result of `pane.process_info` (bd-1np). `foreground_processes` lists the
/// processes in the pane's foreground process group — a live agent process is
/// the group leader (`pid == foreground_process_group_id`); the `shell_pid`
/// is the pane's shell, which reclaims the foreground after the agent exits.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PaneProcessInfo {
    pub pane_id: String,
    #[serde(default)]
    pub foreground_process_group_id: Option<u64>,
    #[serde(default)]
    pub shell_pid: Option<u64>,
    #[serde(default)]
    pub foreground_processes: Vec<PaneProcessInfoProcess>,
}

/// `pane.process_info` — delegation over `Client::call`.
pub fn pane_process_info(c: &mut Client, pane_id: &str) -> Result<PaneProcessInfo> {
    let res = c.call("pane.process_info", json!({ "pane_id": pane_id }))?;
    serde_json::from_value(res.get("process_info").cloned().unwrap_or(res))
        .context("parse pane.process_info result")
}

/// Known agent-kind → process-name mapping for the liveness probe (bd-1np).
/// These are the canonical executables herdr launches per kind
/// (`herdr agent start --kind <KIND>`); the foreground process's `name` /
/// `argv0` matches one of these while the agent lives.
pub fn kind_process_names(kind: &str) -> &'static [&'static str] {
    match kind {
        "opencode" => &["opencode"],
        "pi" => &["pi", "gemini"],
        "grok" => &["grok", "grok-build"],
        "omp" => &["omp"],
        // unknown kind — no process-name mapping; the caller must treat the
        // probe as inconclusive rather than false-negative "healthy".
        _ => &[],
    }
}

/// Pure liveness verdict from a `pane.process_info` snapshot (bd-1np).
///
/// The agent process is alive iff its binary appears in the pane's foreground
/// processes (herdr's process-info lists the whole foreground group; the live
/// agent is its leader). Rules:
/// - binary match → `Some(true)` (ground truth, no other check needed);
/// - no binary match AND the pane's shell owns the foreground group
///   (`fgpgid == shell_pid`) → the agent is gone and the pane returned to the
///   shell prompt → `Some(false)`;
/// - no binary match, foreground NOT shell-owned (e.g. orphaned MCP server
///   children linger after the agent died) → the agent binary is absent →
///   `Some(false)` for a known kind;
/// - unknown kind (no process-name mapping) with no other signal → `None`
///   (inconclusive — the caller falls back to legacy pane-existence
///   semantics; a dead pane must never be confidently "healthy").
pub fn pane_agent_alive(info: &PaneProcessInfo, kind: &str) -> Option<bool> {
    let names = kind_process_names(kind);
    let binary_hit = names.iter().any(|n| {
        info.foreground_processes
            .iter()
            .any(|p| p.name == *n || p.argv0.as_deref() == Some(*n))
    });
    if binary_hit {
        return Some(true);
    }
    let shell_owns_foreground = match (info.foreground_process_group_id, info.shell_pid) {
        (Some(fg), Some(shell)) => fg == shell,
        _ => false,
    };
    if shell_owns_foreground {
        return Some(false);
    }
    if names.is_empty() {
        return None;
    }
    Some(false)
}

/// Spawn-to-dispatch readiness gate (bd-eign.6): poll `pane.process_info`
/// until the agent's binary appears in the pane's foreground process group.
///
/// herdr's `agent.start` returns once the launch command is dispatched, NOT
/// when the agent TUI is ready to receive input — during the 2-30s boot
/// window (config load, MCP server spawn, model connect) the foreground
/// process is the shell or a startup command, and a prompt dispatched in
/// that window is lost to the shell. The agent process becomes the
/// foreground group leader exactly when its TUI owns the pane, so its
/// presence is the readiness signal.
///
/// Polls every 500ms until the agent appears (→ `Ok(true)`), the probe
/// errors continuously, or `timeout` elapses (→ `Ok(false)` — the caller
/// warns; the pane may still boot late, but dispatch now risks the race).
pub fn wait_agent_foreground(
    c: &mut Client,
    pane_id: &str,
    kind: &str,
    name: &str,
    timeout: Duration,
) -> Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last_err: Option<String> = None;
    while std::time::Instant::now() < deadline {
        match pane_process_info(c, pane_id) {
            Ok(pi) => {
                if pane_agent_alive(&pi, kind) == Some(true) {
                    return Ok(true);
                }
            }
            Err(e) => last_err = Some(format!("{e:#}")),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if let Some(e) = last_err {
        eprintln!("flywheel: readiness gate for {name} ({pane_id}): last probe error: {e}");
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Responsiveness probe (bd-a8i): `state_change_seq` over `agent.list`.
// ---------------------------------------------------------------------------

/// Responsiveness verdict for a live-but-possibly-frozen agent (bd-a8i).
///
/// `pane_agent_alive` proves the process exists and owns the foreground
/// group; a frozen agent (stuck TUI, no event-loop progress) still passes
/// that check. Responsiveness uses herdr's per-pane `state_change_seq`: a
/// live agent's state counter advances on every state change, so a seq that
/// does NOT advance across a full window while the agent reports Idle is a
/// frozen process — provably unresponsive, needs respawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Responsiveness {
    Responsive,
    Unresponsive,
    Inconclusive,
}

/// Pure verdict from two `state_change_seq` samples + idle status (bd-a8i).
///
/// - seq advanced → `Responsive` (state demonstrably changed);
/// - seq regressed → `Inconclusive` (the counter is monotonic per pane; a
///   regression means the samples are unreliable);
/// - window too short (`elapsed_secs == 0`) → `Inconclusive` (no time to
///   observe a change — never declare frozen on a sub-second window);
/// - seq frozen + agent Idle → `Unresponsive` (alive as a PID, dead to
///   input — the exact bd-a8i shape);
/// - seq frozen + agent not Idle → `Inconclusive` (Working/Blocked agents
///   may legitimately hold the counter; never respawn on ambiguous evidence).
pub fn responsiveness_verdict(
    old_seq: u64,
    new_seq: u64,
    elapsed_secs: u64,
    idle: bool,
) -> Responsiveness {
    if new_seq > old_seq {
        return Responsiveness::Responsive;
    }
    if new_seq < old_seq || elapsed_secs == 0 {
        return Responsiveness::Inconclusive;
    }
    if idle {
        Responsiveness::Unresponsive
    } else {
        Responsiveness::Inconclusive
    }
}

/// Two-sample responsiveness probe over `agent.list` (bd-a8i): read the
/// pane's `state_change_seq` + idle status, wait `window`, read again, then
/// classify with the pure verdict. `Inconclusive` when either sample is
/// unavailable (agent vanished mid-probe) — the caller must NOT respawn on
/// inconclusive evidence.
pub fn probe_agent_responsiveness(
    c: &mut Client,
    pane_id: &str,
    window: Duration,
) -> Responsiveness {
    let sample = |c: &mut Client| -> Option<(u64, bool)> {
        let agents = c.agent_list().ok()?;
        let a = agents.iter().find(|a| a.pane_id == pane_id)?;
        Some((a.state_change_seq, a.agent_status == AgentStatus::Idle))
    };
    let Some((old_seq, idle)) = sample(c) else {
        return Responsiveness::Inconclusive;
    };
    std::thread::sleep(window);
    let Some((new_seq, _)) = sample(c) else {
        return Responsiveness::Inconclusive;
    };
    responsiveness_verdict(old_seq, new_seq, window.as_secs(), idle)
}

// ---------------------------------------------------------------------------
// Subscription stream adapter.
// ---------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)] // Event carries the typed push payload by design
enum StreamItem {
    Event(PushEvent),
    Closed,
    Error(String),
}

/// A bounded subscription stream. The toron-core `EventStream` runs on a
/// dedicated reader thread; `set_read_timeout` bounds each `read_event` via
/// `recv_timeout` (the toron-core stream has no socket handle to set
/// SO_RCVTIMEO on, so the bound moves to the channel — same observable
/// behavior: a quiet stream's `read_event` returns Err after the deadline).
pub struct EventStream {
    rx: std::sync::mpsc::Receiver<StreamItem>,
    timeout: Option<Duration>,
}

/// Subscribe to herdr push events, returning the flywheel-shaped stream.
/// Delegation: the subscription handshake + socket lifecycle are entirely
/// toron-core's.
pub fn subscribe(c: &mut Client, subscriptions: &[Subscription]) -> Result<EventStream> {
    let inner = c
        .subscribe(subscriptions)
        .map_err(|e| anyhow!("herdr subscribe failed: {e}"))?;
    let (tx, rx) = std::sync::mpsc::channel::<StreamItem>();
    std::thread::spawn(move || {
        let mut inner = inner;
        loop {
            match inner.read_event() {
                Ok(Some((kind, data))) => {
                    let ev = match HerdrEventKind::parse(&kind) {
                        Ok(k) => match HerdrEventData::from_kind(k, data) {
                            Ok(data) => PushEvent { kind: k, data },
                            Err(e) => {
                                let _ = tx.send(StreamItem::Error(format!(
                                    "malformed herdr event {kind}: {e}"
                                )));
                                break;
                            }
                        },
                        Err(e) => {
                            let _ = tx.send(StreamItem::Error(format!(
                                "unknown herdr event kind {kind}: {e}"
                            )));
                            break;
                        }
                    };
                    if tx.send(StreamItem::Event(ev)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(StreamItem::Closed);
                    break;
                }
                Err(e) => {
                    let _ = tx.send(StreamItem::Error(format!("herdr read failed: {e}")));
                    break;
                }
            }
        }
    });
    Ok(EventStream { rx, timeout: None })
}

impl EventStream {
    /// Read one pushed event. `Ok(None)` when the server closes the stream;
    /// `Err` on a read timeout (when a deadline was set) or transient I/O.
    pub fn read_event(&mut self) -> Result<Option<PushEvent>> {
        let item = match self.timeout {
            Some(d) => self.rx.recv_timeout(d).map_err(|e| match e {
                std::sync::mpsc::RecvTimeoutError::Timeout => {
                    anyhow!("herdr stream read timed out")
                }
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    anyhow!("herdr stream closed")
                }
            }),
            None => self.rx.recv().map_err(|_| anyhow!("herdr stream closed")),
        }?;
        match item {
            StreamItem::Event(e) => Ok(Some(e)),
            StreamItem::Closed => Ok(None),
            StreamItem::Error(msg) => Err(anyhow!("{msg}")),
        }
    }

    /// Bound (or unbound) each blocking read. `None` restores blocking reads.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.timeout = timeout;
        Ok(())
    }
}

#[cfg(test)]
mod process_liveness_tests {
    use super::{PaneProcessInfo, PaneProcessInfoProcess, kind_process_names, pane_agent_alive};

    fn proc(name: &str, pid: u64) -> PaneProcessInfoProcess {
        PaneProcessInfoProcess {
            pid,
            name: name.to_string(),
            argv0: Some(name.to_string()),
        }
    }

    fn info(
        fgpgid: Option<u64>,
        shell_pid: Option<u64>,
        procs: Vec<PaneProcessInfoProcess>,
    ) -> PaneProcessInfo {
        PaneProcessInfo {
            pane_id: "w1:p1".to_string(),
            foreground_process_group_id: fgpgid,
            shell_pid,
            foreground_processes: procs,
        }
    }

    /// Live agent: the agent binary leads the foreground group (pid == fgpgid).
    #[test]
    fn live_opencode_classified_alive() {
        let i = info(
            Some(100),
            Some(50),
            vec![proc("opencode", 100), proc("node", 101), proc("uv", 102)],
        );
        assert_eq!(pane_agent_alive(&i, "opencode"), Some(true));
    }

    /// Dead process: pane exists but the shell owns the foreground — the
    /// exact bd-1np false-negative shape (pane present, process gone).
    #[test]
    fn dead_process_shell_foreground_classified_dead() {
        let i = info(Some(50), Some(50), vec![proc("zsh", 50)]);
        assert_eq!(pane_agent_alive(&i, "opencode"), Some(false));
    }

    /// Dead process with orphaned children still listed (MCP servers linger
    /// after the agent died): no agent binary, foreground not shell-owned.
    #[test]
    fn dead_process_orphan_children_classified_dead() {
        let i = info(
            Some(103),
            Some(50),
            vec![proc("node", 103), proc("uv", 104)],
        );
        assert_eq!(pane_agent_alive(&i, "opencode"), Some(false));
    }

    /// Empty foreground list: nothing running.
    #[test]
    fn empty_foreground_classified_dead() {
        let i = info(Some(50), Some(50), vec![]);
        assert_eq!(pane_agent_alive(&i, "opencode"), Some(false));
    }

    /// Unknown kind with no other signal: inconclusive, never "healthy".
    #[test]
    fn unknown_kind_inconclusive() {
        let i = info(Some(200), Some(50), vec![proc("mystery-agent", 200)]);
        assert_eq!(pane_agent_alive(&i, "mystery-kind"), None);
    }

    /// Unknown kind but shell owns the foreground: still provably dead.
    #[test]
    fn unknown_kind_shell_foreground_dead() {
        let i = info(Some(50), Some(50), vec![proc("zsh", 50)]);
        assert_eq!(pane_agent_alive(&i, "mystery-kind"), Some(false));
    }

    /// Kind aliases: grok's binary can be grok-build.
    #[test]
    fn kind_alias_matches() {
        let i = info(Some(300), Some(50), vec![proc("grok-build", 300)]);
        assert_eq!(pane_agent_alive(&i, "grok"), Some(true));
    }

    /// Argv0-only match (name may be a comm name like python3.14t).
    #[test]
    fn argv0_match_counts() {
        let i = info(
            Some(400),
            Some(50),
            vec![PaneProcessInfoProcess {
                pid: 400,
                name: "opencode".to_string(),
                argv0: Some("/opt/bin/opencode".to_string()),
            }],
        );
        assert_eq!(pane_agent_alive(&i, "opencode"), Some(true));
    }

    #[test]
    fn kind_names_mapping_has_all_four() {
        assert!(kind_process_names("opencode").contains(&"opencode"));
        assert!(kind_process_names("pi").contains(&"pi"));
        assert!(kind_process_names("grok").contains(&"grok"));
        assert!(kind_process_names("omp").contains(&"omp"));
    }
}

#[cfg(test)]
mod sanitize_pane_text_tests {
    use super::sanitize_pane_text;

    #[test]
    fn strips_csi_color_sequences() {
        assert_eq!(
            sanitize_pane_text("\u{1b}[31mred\u{1b}[0m and \u{1b}[1;32mbold green\u{1b}[m"),
            "red and bold green"
        );
    }

    #[test]
    fn strips_csi_cursor_and_erase_sequences() {
        assert_eq!(
            sanitize_pane_text("a\u{1b}[2Kb\u{1b}[1;1Hc"),
            "abc"
        );
    }

    #[test]
    fn strips_osc_hyperlink_with_st_terminator() {
        assert_eq!(
            sanitize_pane_text("\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\more"),
            "linkmore"
        );
    }

    #[test]
    fn strips_osc_title_with_bel_terminator() {
        assert_eq!(
            sanitize_pane_text("before\u{1b}]0;term-title\u{07}after"),
            "beforeafter"
        );
    }

    #[test]
    fn strips_one_shot_escapes() {
        assert_eq!(
            sanitize_pane_text("abc\u{1b}7def\u{1b}8ghi\u{1b}=j"),
            "abcdefghij"
        );
    }

    #[test]
    fn strips_box_drawing_border_glyphs() {
        let border = "┏━━━━━━┓\n┃ ok   ┃\n┗━━━━━━┛";
        assert_eq!(sanitize_pane_text(border), "\n ok   \n");
    }

    #[test]
    fn strips_double_line_and_heavy_box_glyphs() {
        assert_eq!(sanitize_pane_text("╔═══╗ ║ ╠ ╣ ╦ ╩ ╬"), "      ");
    }

    #[test]
    fn strips_block_element_and_shade_glyphs() {
        assert_eq!(sanitize_pane_text("▀▄█░▒▓text"), "text");
    }

    #[test]
    fn mixed_garbled_tui_output_becomes_readable() {
        let raw = "\u{1b}[2J\u{1b}[H╔═╗ \u{1b}[1;32mREADY\u{1b}[0m ═╝\u{1b}[?25l";
        assert_eq!(sanitize_pane_text(raw), " READY ");
    }

    #[test]
    fn plain_content_unchanged() {
        assert_eq!(sanitize_pane_text("hello world"), "hello world");
    }

    #[test]
    fn empty_string_unchanged() {
        assert_eq!(sanitize_pane_text(""), "");
    }

    #[test]
    fn unterminated_csi_dropped() {
        assert_eq!(sanitize_pane_text("lead\u{1b}[31m"), "lead");
    }
}

#[cfg(test)]
mod responsiveness_tests {
    use super::{Responsiveness, responsiveness_verdict};

    /// Seq advanced across the window: state changed, so the agent is
    /// responsive whatever the idle status or window length.
    #[test]
    fn advanced_seq_responsive() {
        assert_eq!(
            responsiveness_verdict(10, 11, 3, true),
            Responsiveness::Responsive
        );
        assert_eq!(
            responsiveness_verdict(10, 11, 0, true),
            Responsiveness::Responsive
        );
        assert_eq!(
            responsiveness_verdict(10, 42, 3, false),
            Responsiveness::Responsive
        );
    }

    /// The bd-a8i shape: seq frozen + Idle across a full window — alive as a
    /// PID, unresponsive to input, needs respawn.
    #[test]
    fn frozen_seq_idle_unresponsive() {
        assert_eq!(
            responsiveness_verdict(10, 10, 3, true),
            Responsiveness::Unresponsive
        );
    }

    /// Frozen seq but the agent reports non-idle: Working/Blocked agents may
    /// legitimately hold the counter — never respawn on ambiguous evidence.
    #[test]
    fn frozen_seq_busy_inconclusive() {
        assert_eq!(
            responsiveness_verdict(10, 10, 3, false),
            Responsiveness::Inconclusive
        );
    }

    /// Sub-second window: not enough time to observe a change — a frozen
    /// agent must not be declared from a near-zero window.
    #[test]
    fn zero_window_inconclusive() {
        assert_eq!(
            responsiveness_verdict(10, 10, 0, true),
            Responsiveness::Inconclusive
        );
    }

    /// Seq regression: the per-pane counter is monotonic; a regression means
    /// the samples are unreliable, so the verdict is inconclusive.
    #[test]
    fn regressed_seq_inconclusive() {
        assert_eq!(
            responsiveness_verdict(10, 9, 3, true),
            Responsiveness::Inconclusive
        );
    }
}
