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

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use toron_core::backend::error::{BackendError, BackendErrorCode};
pub use toron_core::backend::herdr::{
    AgentInfo, AgentSessionInfo, AgentSessionRefKind, Client, HDR_PROTOCOL, PaneInfo, SendVerdict,
    Subscription, WaitOptions, WorkspaceInfo, classify_send_result,
};
pub use toron_core::backend::types::AgentStatus;

/// GS-049 / bd-a2bw: herdr `launch_pending` is stale when the pane is already
/// input-ready (idle/working/blocked/done). Shared by snapshot assemble and
/// spawn/send readiness so live decide Snapshot never invent-greens 0.
pub fn agent_still_booting(a: &AgentInfo) -> bool {
    if !a.launch_pending {
        return false;
    }
    if a.interactive_ready
        && matches!(
            a.agent_status,
            AgentStatus::Idle | AgentStatus::Working | AgentStatus::Blocked | AgentStatus::Done
        )
    {
        return false;
    }
    true
}

/// bd-bug-omp-herdr-wedge: the HERDR_* integration env a herdr-launched
/// harness needs to activate its herdr extension (omp's
/// herdr-omp-agent-state.ts gates on HERDR_ENV=1 + socket + pane id).
/// herdr's agent.start has NO env param and does not inherit pane env
/// (direct exec), so the caller must inject at pane creation:
/// workspace.create / pane.split both accept env. herdr fills the per-pane
/// ids (HERDR_PANE_ID/TAB/WORKSPACE_ID) itself at agent.start; we supply
/// the two workspace-level vars.
///
/// bd-pin-public-plane-s-easy.2: the pane env additionally carries the
/// agent's mail PIN (`AGENT_NAME` + `FLYWHEEL_MAIL_AS`) so a spawned agent
/// boots knowing the identity it is addressed by — same pin the peer
/// template injects and the loop's `{AGENT_NAME}` substitutes.
pub fn herdr_integration_env(
    cfg: &crate::config::Config,
    pin: Option<&str>,
) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::from([
        ("HERDR_ENV".to_string(), "1".to_string()),
        ("HERDR_SOCKET_PATH".to_string(), cfg.socket_path()),
    ]);
    if let Some(pin) = pin.filter(|p| !p.is_empty()) {
        env.insert("AGENT_NAME".to_string(), pin.to_string());
        env.insert("FLYWHEEL_MAIL_AS".to_string(), pin.to_string());
    }
    // GS-037 P0-4: Captain secret name OPENCODE_GO_API_KEY must reach panes as
    // OPENCODE_API_KEY (pi/opencode-go catalog). Alias when GO is set and API
    // unset — never log the value.
    apply_opencode_go_api_key_alias(&mut env);
    env
}

/// GS-037 P0-4: if `OPENCODE_GO_API_KEY` is set in the process env and
/// `OPENCODE_API_KEY` is unset (process + pane map), copy the bytes under the
/// API name into the pane env so opencode-go models authenticate.
pub fn apply_opencode_go_api_key_alias(env: &mut std::collections::BTreeMap<String, String>) {
    if env.contains_key("OPENCODE_API_KEY") {
        return;
    }
    if std::env::var_os("OPENCODE_API_KEY").is_some() {
        return;
    }
    if let Ok(v) = std::env::var("OPENCODE_GO_API_KEY")
        && !v.is_empty()
    {
        env.insert("OPENCODE_API_KEY".to_string(), v);
        // Also keep the GO name so auth.json `$OPENCODE_GO_API_KEY` refs work.
        if !env.contains_key("OPENCODE_GO_API_KEY")
            && let Ok(go) = std::env::var("OPENCODE_GO_API_KEY")
        {
            env.insert("OPENCODE_GO_API_KEY".to_string(), go);
        }
    }
}

/// bd-crlx: pin an opencode TUI agent's model for one pane.
///
/// opencode v2's ROOT command (the one `herdr agent start --kind opencode`
/// launches) has no `--model` flag — the flag exists only on `opencode run`.
/// The TUI's model is config, so the pin rides the pane env as
/// `OPENCODE_CONFIG_CONTENT`.
///
/// Why this is safe: opencode MERGES config sources rather than replacing
/// them. Measured on v2.0.16 via `opencode api --standalone GET /api/config`:
/// with the var set, the resolved config gains a THIRD `document` source
/// carrying `model`, while the global `opencode.json` keeps its
/// `permissions`, `agents`, and `mcp` intact, and the pinned model wins
/// (last document wins per key). So this never clobbers the operator's
/// agents/mcp/permissions — the same pane-env channel
/// [`apply_opencode_go_api_key_alias`] already uses for opencode-go auth.
///
/// Two refusals, both fail-closed:
/// - a model that is not `provider/model` is rejected. opencode's own parser
///   drops such a reference as an "unsupported legacy model reference", which
///   would silently UNPIN the seat rather than error;
/// - pre-existing `OPENCODE_CONFIG_CONTENT` is MERGED into, never replaced,
///   and unparseable content is an error rather than an overwrite. An operator
///   who set that var keeps their other settings.
pub fn apply_opencode_model_pin(
    env: &mut std::collections::BTreeMap<String, String>,
    model: &str,
) -> anyhow::Result<()> {
    // opencode's model reference grammar: provider ends at the first `/`,
    // model may contain `/`, neither may contain `#` in this position.
    if crate::models::split_key(model).0.is_empty() {
        anyhow::bail!(
            "opencode model pin {model:?} is not in provider/model form — opencode drops \
             such a reference instead of selecting it, which would leave the seat unpinned"
        );
    }
    const KEY: &str = "OPENCODE_CONFIG_CONTENT";
    let mut doc: serde_json::Map<String, serde_json::Value> = match env.get(KEY) {
        Some(existing) if !existing.trim().is_empty() => {
            let parsed: serde_json::Value = serde_json::from_str(existing).map_err(|e| {
                anyhow::anyhow!(
                    "pane env {KEY} is set but is not valid JSON ({e}) — refusing to overwrite \
                     it to pin model {model:?}; fix or unset it for this shell"
                )
            })?;
            match parsed {
                serde_json::Value::Object(o) => o,
                other => anyhow::bail!(
                    "pane env {KEY} is set to a non-object JSON {other} — refusing to \
                     overwrite it to pin model {model:?}"
                ),
            }
        }
        _ => serde_json::Map::new(),
    };
    doc.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    env.insert(KEY.to_string(), serde_json::Value::Object(doc).to_string());
    Ok(())
}

/// GS-037 P1: herdr `integration.install` fails when parent config dirs are
/// missing (pi/omp/opencode). Create the documented parent dirs before install
/// so zero-touch spawn works on a cold box.
pub fn ensure_integration_parent_dirs(kind: &str) {
    // P4: authority injects — one HOME read in HostAuthority, not per site.
    let home = std::path::PathBuf::from(crate::authority::HostAuthority::resolve().home);
    let dirs: &[&str] = match kind {
        "pi" => &[".pi/agent/extensions"],
        "opencode" => &[".config/opencode/plugins"],
        _ => &[],
    };
    for rel in dirs {
        let p = home.join(rel);
        if let Err(e) = std::fs::create_dir_all(&p) {
            eprintln!(
                "flywheel: warn: cannot create {} for {kind} integration install: {e}",
                p.display()
            );
        }
    }
}

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

/// A herdr agent integration's install state, parsed from a
/// `herdr integration status` line (`<kind>: <state> (<path>)`).
///
/// H3 (bd-harness-integration-preflight): spawn preflights each launched
/// kind's integration and installs it when missing/outdated; `flywheel
/// status` / `flywheel deps` surface this state per kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationStatus {
    /// integration file exists at the documented path
    pub installed: bool,
    /// installed version equals the version herdr ships (vN == expected)
    pub current: bool,
    /// installed version (`vN`) when known; `None` when not installed
    pub version: Option<String>,
    /// expected version (`vM`) for `outdated` lines; `None` otherwise
    pub expected_version: Option<String>,
    /// install path (the trailing `(...)` group); `None` when unparseable
    pub path: Option<String>,
}

impl IntegrationStatus {
    /// The herdr default for any kind with no installed integration.
    pub fn not_installed() -> Self {
        Self {
            installed: false,
            current: false,
            version: None,
            expected_version: None,
            path: None,
        }
    }
}

/// Canonical herdr integration kinds (`herdr integration status` /
/// `herdr integration install` surface, herdr.dev/docs/preview/integrations/
/// build 2026-08-04-d78e3d3b5126). Flywheel custom kinds without a herdr
/// integration are NOT in this list — preflight skips them (nothing to
/// install, nothing to report).
pub const INTEGRATION_KINDS: &[&str] = &[
    "pi",
    "claude",
    "codex",
    "copilot",
    "devin",
    "droid",
    "kimi",
    "opencode",
    "kilo",
    "hermes",
    "qodercli",
    "cursor",
    "mastracode",
    "antigravity-cli",
    "grok",
];

/// Map a flywheel kind to the socket `integration.install` target (the
/// `IntegrationTarget` enum). `None` when the kind has no herdr integration.
/// The only spelling difference: `antigravity-cli` (CLI display) vs
/// `antigravity_cli` (socket enum).
pub fn integration_target(kind: &str) -> Option<&'static str> {
    if kind == "antigravity-cli" {
        return Some("antigravity_cli");
    }
    INTEGRATION_KINDS.iter().copied().find(|k| *k == kind)
}

/// Parse one line of `herdr integration status` output into `(kind, status)`.
/// `None` for a line that does not match the `<kind>: <state> (<path>)`
/// grammar herdr 0.8.0-preview emits. States:
/// - `current (v9) (/path)`        → installed + current
/// - `outdated (v3 < v4) (/path)`  → installed, not current
/// - `not installed (/path)`       → not installed
pub fn parse_integration_status_line(line: &str) -> Option<(&str, IntegrationStatus)> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (kind, rest) = line.split_once(": ")?;
    let kind = kind.trim();
    // The path is the LAST ` (...) ` group; state text precedes it.
    let (state_part, path) = match rest.rfind(" (") {
        Some(idx) if rest[idx..].ends_with(')') => (
            &rest[..idx],
            Some(rest[idx + 2..rest.len() - 1].to_string()),
        ),
        _ => (rest, None),
    };
    let (installed, current, version, expected_version) = if let Some(inner) = state_part
        .strip_prefix("current (")
        .and_then(|s| s.strip_suffix(')'))
    {
        (true, true, Some(inner.to_string()), None)
    } else if let Some(inner) = state_part
        .strip_prefix("outdated (")
        .and_then(|s| s.strip_suffix(')'))
    {
        let (v, exp) = inner.split_once(" < ").unwrap_or((inner, ""));
        (
            true,
            false,
            Some(v.to_string()),
            (!exp.is_empty()).then(|| exp.to_string()),
        )
    } else if state_part.trim() == "not installed" {
        (false, false, None, None)
    } else {
        // Unknown state text — shape drift: treat as not installed and
        // drop the path claim rather than inventing a state.
        (false, false, None, None)
    };
    Some((
        kind,
        IntegrationStatus {
            installed,
            current,
            version,
            expected_version,
            path,
        },
    ))
}

/// Parse full `herdr integration status` output into per-kind statuses
/// (one call, N kinds — spawn/deps reuse the map). Kinds absent from the
/// output are omitted; callers fall back to [`IntegrationStatus::not_installed`].
pub fn parse_integration_status_output(raw: &str) -> Vec<(String, IntegrationStatus)> {
    raw.lines()
        .filter_map(parse_integration_status_line)
        .map(|(k, s)| (k.to_string(), s))
        .collect()
}

/// Run `herdr integration status` once and return every kind's state.
pub fn integration_status_all() -> Result<Vec<(String, IntegrationStatus)>> {
    let out = std::process::Command::new("herdr")
        .args(["integration", "status"])
        .output()
        .map_err(|e| anyhow!("cannot run `herdr integration status`: {e}"))?;
    if !out.status.success() {
        bail!(
            "`herdr integration status` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_integration_status_output(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

/// Install a kind's herdr integration through the socket `integration.install`
/// method (herdr 0.8.0-preview raw method; the socket is the in-process
/// surface — `herdr integration install <TARGET>` is the CLI twin). Returns
/// the server's install messages. The CALLER decides how to surface
/// failures — H3 preflight warns loudly and never fails the spawn.
pub fn integration_install_socket(c: &mut Client, kind: &str) -> Result<Vec<String>> {
    let target = integration_target(kind)
        .ok_or_else(|| anyhow!("kind {kind} has no herdr integration target"))?;
    // GS-037 P1: mkdir parent dirs before socket install (pi/omp/opencode).
    ensure_integration_parent_dirs(target);
    let resp = c.call("integration.install", json!({ "target": target }))?;
    let messages = resp
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(messages)
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
    // bd-ws-resolution (2026-08-17): prefer the flywheel-OWNED workspace
    // LABEL over a foreign workspace sharing the bare project label — herdr
    // reports no names for omp panes, so the agent-name scan below found
    // nothing and dispatch fell to list order, targeting a foreign
    // workspace with zero flywheel agents ("no agents in workspace w37").
    // spawn names its own workspaces `flywheel-<base>`; the exact label
    // (or its hyphen-prefixed supersets) is the deterministic tie-breaker.
    let owned = prefix.trim_end_matches('-');
    let owned_prefix = format!("{owned}-");
    for ws in matches {
        if ws.label == owned || ws.label.starts_with(&owned_prefix) {
            return ws.workspace_id.clone();
        }
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

/// The workspace holding the project's REGISTERED panes.
///
/// A spawned crew's herdr name is its MAIL PIN (the spawn names agents by the
/// identity they were given), so the prefix scan in [`resolve_workspace_id`]
/// cannot see it and the tie falls to list order — the stale room that merely
/// carries the bare project label. The spawn registry is the project→pane
/// map, so the panes it names identify the live swarm: a stale room holds no
/// registered pane. Returns `None` when no registered pane is in one of the
/// candidate workspaces, so the caller keeps its existing resolution.
/// Deterministic — ties keep the candidate list's order.
pub fn resolve_workspace_from_registry(
    matches: &[WorkspaceInfo],
    agents: &[AgentInfo],
    registered_panes: &[String],
) -> Option<String> {
    if registered_panes.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &str)> = None;
    for ws in matches {
        let n = agents
            .iter()
            .filter(|a| {
                a.workspace_id == ws.workspace_id
                    && registered_panes.iter().any(|p| p == &a.pane_id)
            })
            .count();
        if n > 0 && best.map(|(bn, _)| n > bn).unwrap_or(true) {
            best = Some((n, &ws.workspace_id));
        }
    }
    best.map(|(_, id)| id.to_string())
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
        use HerdrEventKind::{
            LayoutUpdated, PaneAgentDetected, PaneAgentStatusChanged, PaneAgentStatusChangedSub,
            PaneClosed, PaneCreated, PaneExited, PaneFocused, PaneMoved, PaneOutputChanged,
            PaneOutputMatched, PaneScrollChanged, PaneUpdated, TabClosed, TabCreated, TabFocused,
            TabMoved, TabRenamed, WorkspaceClosed, WorkspaceCreated, WorkspaceFocused,
            WorkspaceMetadataUpdated, WorkspaceMoved, WorkspaceRenamed, WorkspaceReordered,
            WorkspaceUpdated, WorktreeCreated, WorktreeOpened, WorktreeRemoved,
        };
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
        use HerdrEventKind::{
            LayoutUpdated, PaneAgentDetected, PaneAgentStatusChanged, PaneAgentStatusChangedSub,
            PaneClosed, PaneCreated, PaneExited, PaneFocused, PaneMoved, PaneOutputChanged,
            PaneOutputMatched, PaneScrollChanged, PaneUpdated, TabClosed, TabCreated, TabFocused,
            TabMoved, TabRenamed, WorkspaceClosed, WorkspaceCreated, WorkspaceFocused,
            WorkspaceMetadataUpdated, WorkspaceMoved, WorkspaceRenamed, WorkspaceReordered,
            WorkspaceUpdated, WorktreeCreated, WorktreeOpened, WorktreeRemoved,
        };
        use serde_json::from_value;
        Ok(match kind {
            WorkspaceCreated => Self::WorkspaceCreated(from_value(data)?),
            WorkspaceUpdated => Self::WorkspaceUpdated(from_value(data)?),
            WorkspaceMetadataUpdated => Self::WorkspaceMetadataUpdated(from_value(data)?),
            WorkspaceClosed => Self::WorkspaceClosed(from_value(data)?),
            WorkspaceRenamed => Self::WorkspaceRenamed(from_value(data)?),
            WorkspaceMoved => Self::WorkspaceMoved(from_value(data)?),
            WorkspaceReordered => Self::WorkspaceReordered(from_value(data)?),
            WorkspaceFocused => Self::WorkspaceFocused(from_value(data)?),
            WorktreeCreated => Self::WorktreeCreated(from_value(data)?),
            WorktreeOpened => Self::WorktreeOpened(from_value(data)?),
            WorktreeRemoved => Self::WorktreeRemoved(from_value(data)?),
            TabCreated => Self::TabCreated(from_value(data)?),
            TabClosed => Self::TabClosed(from_value(data)?),
            TabRenamed => Self::TabRenamed(from_value(data)?),
            TabMoved => Self::TabMoved(from_value(data)?),
            TabFocused => Self::TabFocused(from_value(data)?),
            PaneCreated => Self::PaneCreated(from_value(data)?),
            PaneClosed => Self::PaneClosed(from_value(data)?),
            PaneUpdated => Self::PaneUpdated(from_value(data)?),
            PaneFocused => Self::PaneFocused(from_value(data)?),
            PaneMoved => Self::PaneMoved(from_value(data)?),
            PaneOutputChanged => Self::PaneOutputChanged(from_value(data)?),
            PaneExited => Self::PaneExited(from_value(data)?),
            PaneAgentDetected => Self::PaneAgentDetected(from_value(data)?),
            PaneAgentStatusChanged => Self::PaneAgentStatusChanged(from_value(data)?),
            LayoutUpdated => Self::LayoutUpdated(from_value(data)?),
            PaneAgentStatusChangedSub => Self::PaneAgentStatusChangedSub(from_value(data)?),
            PaneOutputMatched => Self::PaneOutputMatched(from_value(data)?),
            PaneScrollChanged => Self::PaneScrollChanged(from_value(data)?),
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
            });
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
    env: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<PaneInfo> {
    let mut params = serde_json::Map::new();
    params.insert("target_pane_id".into(), serde_json::json!(pane_id));
    params.insert("direction".into(), serde_json::json!(direction));
    params.insert("cwd".into(), serde_json::json!(cwd));
    if let Some(env) = env {
        // ADR-0010 D13: every pane.split pane is a CREW pane (the captain
        // rides the root pane's workspace-level env). Stamp the role so the
        // subagent-guard fence exempts crew: the deny list must disarm
        // nobody it does not own. Caller-supplied wins on collision.
        let mut env = env.clone();
        env.entry("FLYWHEEL_ROLE".to_string())
            .or_insert_with(|| "crew".to_string());
        params.insert("env".into(), serde_json::json!(env));
    }
    let res = c.call("pane.split", serde_json::Value::Object(params))?;
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

/// Spawn shell-readiness gate (bd-pattern-peer-triggered): poll a
/// freshly-split pane until its shell has rendered a prompt, so the launch
/// command lands in an INTERACTIVE shell — not a booting one.
///
/// Why this gate exists: herdr's `agent.start` types the launch command
/// into the pane's PTY immediately after `pane.split` returns. A new
/// pane's shell is still starting up (zsh running `.zshrc`, prompt not yet
/// rendered) and the PTY is in CANONICAL mode with a 1024-byte line limit
/// (`MAX_CANON`). The peer-aware template renders to a ~2.6KB single-quoted
/// launch arg; typing it into a booting shell splits it mid-quote, and zsh
/// parks at the PS2 continuation prompt (`>....`) with the agent never
/// starting (2026-08-18 coder2/coder3 dead-on-spawn). The root pane
/// survived only because its first `agent.start` errored (shell not yet an
/// available shell) and the retry's 3s sleep let it finish booting.
///
/// Signal: `pane.read` recent output contains a rendered prompt — a shell
/// prints its prompt exactly when the line editor is up (raw mode, no
/// canonical line limit). Poll every 250ms until the prompt appears (→
/// `Ok(true)`), the probe errors continuously, or `timeout` elapses (→
/// `Ok(false)` — the caller warns; the old race is the fallback).
pub fn wait_shell_prompt(c: &mut Client, pane_id: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let mut last_err: Option<String> = None;
    while std::time::Instant::now() < deadline {
        match pane_recent_text(c, pane_id) {
            Ok(text) => {
                if shell_prompt_rendered(&text) {
                    return true;
                }
            }
            Err(e) => last_err = Some(format!("{e:#}")),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if let Some(e) = last_err {
        eprintln!("flywheel: shell-readiness gate for {pane_id}: last probe error: {e}");
    }
    false
}

/// Recent text of a pane (`pane.read`, source=recent, format=text).
///
/// `pub(crate)`: the pane-wedge probe (bd-pane-wedge-detect-respawn-pnyi)
/// reads the same tail the shell-readiness gate does, so the two can never
/// disagree about what a pane is showing.
pub(crate) fn pane_recent_text(c: &mut Client, pane_id: &str) -> Result<String> {
    let res = c.call(
        "pane.read",
        json!({
            "pane_id": pane_id,
            "source": "recent",
            "lines": 30,
            "format": "text",
            "strip_ansi": true,
        }),
    )?;
    let read = res.get("read").cloned().unwrap_or(res);
    Ok(read
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string())
}

/// True when recent pane output contains a rendered shell prompt. A prompt
/// is a non-empty last line; its exact text is user-custom (zsh
/// `❯`/`%`, bash `$`, root `#`, oh-my-zsh themes), so match the last
/// non-blank line ending in a common prompt sigil OR a `user@host path`
/// shape — either means the shell reached its line editor.
fn shell_prompt_rendered(text: &str) -> bool {
    let last = text.lines().rev().find(|l| !l.trim().is_empty());
    match last {
        Some(line) => {
            let t = line.trim();
            // Common prompt sigils (zsh ❯ / % / $, bash $, root #, fish >,
            // oh-my-zsh ➜) — leading (➜  ~) or trailing (❯).
            if t.starts_with(['❯', '➜', '%', '$', '#', '>', '»', '›', 'λ'])
                || t.ends_with(['❯', '➜', '%', '$', '#', '>', '»', '›', 'λ'])
            {
                return true;
            }
            // user@host:path / user@host dir style prompts.
            t.contains('@') && (t.contains(':') || t.contains('~') || t.contains('/'))
        }
        None => false,
    }
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
        // unknown kind — no process-name mapping; the caller must treat the
        // probe as inconclusive rather than false-negative "healthy".
        _ => &[],
    }
}

/// Does a foreground process name name the expected agent binary?
///
/// Exact match, or the same name carrying an executable suffix — an
/// npm/mise-installed CLI bins its entry point as `<name>.exe` on every
/// platform (measured live 2026-10-07: mise npm-opencode-cli 2.0.24 runs
/// `opencode.exe`), and an exact-name test read those LIVE panes as dead
/// shells (bd-oc-launch-chdir-enoent-1l30).
fn process_name_matches(candidate: &str, expected: &str) -> bool {
    candidate == expected
        || candidate
            .strip_suffix(".exe")
            .is_some_and(|stem| stem == expected)
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
            .any(|p| process_name_matches(&p.name, n) || p.argv0.as_deref() == Some(*n))
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
///
/// herdr's `agent.start` returns once the launch command is dispatched, NOT
/// when the agent TUI is ready to receive input — during the 2-30s boot
/// window (config load, MCP server spawn, model connect) the foreground
/// process is the shell or a startup command, and a prompt dispatched in
/// that window is lost to the shell. The agent process becomes the
/// foreground group leader exactly when its TUI owns the pane, so its
/// presence is the readiness signal.
///
/// Polls every 500ms until the agent appears (→ `true`), the probe
/// errors continuously, or `timeout` elapses (→ `false` — the caller
/// warns; the pane may still boot late, but dispatch now risks the race).
pub fn wait_agent_foreground(
    c: &mut Client,
    pane_id: &str,
    kind: &str,
    name: &str,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let mut last_err: Option<String> = None;
    while std::time::Instant::now() < deadline {
        match pane_process_info(c, pane_id) {
            Ok(pi) => {
                if pane_agent_alive(&pi, kind) == Some(true) {
                    return true;
                }
            }
            Err(e) => last_err = Some(format!("{e:#}")),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if let Some(e) = last_err {
        eprintln!("flywheel: readiness gate for {name} ({pane_id}): last probe error: {e}");
    }
    false
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
    /// herdr subscriber fell behind — typed `events_lost` (toron surfaces it).
    EventsLost,
    Error(String),
}

/// Outcome of one stream read, distinguishing `events_lost` from EOF / I/O.
#[derive(Debug)]
pub enum StreamOutcome {
    /// Boxed on purpose: the push payload is large next to the unit variants
    /// and one of these is returned per read, so the box keeps the enum small
    /// on the reader thread's stack.
    Event(Box<PushEvent>),
    Closed,
    EventsLost,
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
    let mut inner = c
        .subscribe(subscriptions)
        .map_err(|e| anyhow!("herdr subscribe failed: {e}"))?;
    let (tx, rx) = std::sync::mpsc::channel::<StreamItem>();
    std::thread::spawn(move || {
        pump_events(|| inner.read_event(), &tx);
    });
    Ok(EventStream { rx, timeout: None })
}

/// The reader loop, over any source of pushed events.
///
/// A gap (`events_lost`) is a NOTICE, not a death: toron-core's
/// `read_event` returns a typed error and leaves the reader on the open
/// socket, so the stream is still good. This loop used to BREAK on a gap,
/// which killed the subscription: the doorbell then cleared its coverage
/// and re-subscribed from scratch - a NEW socket per gap (the daemon's
/// socket count grew at the gap rate) and a fresh subscriber that starts
/// BEHIND the ring, so it was told `events_lost` again. One upstream gap
/// fanned out into a gap per subscriber and a socket per re-subscribe.
fn pump_events<R>(mut read: R, tx: &std::sync::mpsc::Sender<StreamItem>)
where
    R: FnMut() -> Result<Option<(String, serde_json::Value)>, BackendError>,
{
    loop {
        match read() {
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
            Err(e) if e.code == BackendErrorCode::EventsLost => {
                // In band, and the stream goes on: the caller reconciles the
                // status facts and keeps the subscription.
                if tx.send(StreamItem::EventsLost).is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = tx.send(StreamItem::Error(format!("herdr read failed: {e}")));
                break;
            }
        }
    }
}

impl EventStream {
    /// Read one pushed event. `Ok(None)` when the server closes the stream;
    /// `Err` on a read timeout (when a deadline was set), transient I/O, or
    /// typed `events_lost` (message contains the code — prefer
    /// [`Self::read_outcome`] to classify without string matching).
    pub fn read_event(&mut self) -> Result<Option<PushEvent>> {
        match self.read_outcome()? {
            StreamOutcome::Event(e) => Ok(Some(*e)),
            StreamOutcome::Closed => Ok(None),
            StreamOutcome::EventsLost => Err(anyhow!("events_lost")),
        }
    }

    /// Like [`Self::read_event`] but surfaces `events_lost` as a typed
    /// outcome so callers can clear coverage and snapshot-reconcile.
    pub fn read_outcome(&mut self) -> Result<StreamOutcome> {
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
            StreamItem::Event(e) => Ok(StreamOutcome::Event(Box::new(e))),
            StreamItem::Closed => Ok(StreamOutcome::Closed),
            StreamItem::EventsLost => Ok(StreamOutcome::EventsLost),
            StreamItem::Error(msg) => Err(anyhow!("{msg}")),
        }
    }

    /// Bound (or unbound) each blocking read. `None` restores blocking reads.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.timeout = timeout;
    }
}

#[cfg(test)]
mod events_lost_tests {
    use super::*;

    /// bd-v9k6: a gap is a NOTICE, not a death. The reader loop used to
    /// break on `events_lost`, which killed the subscription: the doorbell
    /// cleared its coverage and re-subscribed (a NEW socket per gap, and the
    /// daemon's socket count grew at the gap rate), and the fresh subscriber
    /// started BEHIND the ring, so it was told `events_lost` again. One
    /// upstream gap fanned out into a gap and a socket per subscriber.
    #[test]
    fn a_gap_is_delivered_in_band_and_the_stream_keeps_reading() {
        let (tx, rx) = std::sync::mpsc::channel::<StreamItem>();
        let mut calls = 0u32;
        pump_events(
            || {
                calls += 1;
                match calls {
                    1 => Err(BackendError::new(
                        BackendErrorCode::EventsLost,
                        "subscriber fell behind",
                    )),
                    2 => Ok(Some((
                        "pane.agent_status_changed".to_string(),
                        serde_json::json!({
                            "pane_id": "w7X:p1",
                            "workspace_id": "w7X",
                            "agent_status": "idle",
                        }),
                    ))),
                    _ => Ok(None),
                }
            },
            &tx,
        );
        let items: Vec<StreamItem> = rx.try_iter().collect();
        assert!(
            matches!(items.first(), Some(StreamItem::EventsLost)),
            "the gap is delivered in band"
        );
        assert!(
            calls >= 3,
            "the loop kept reading past the gap (calls={calls})"
        );
        assert!(
            items.iter().any(|i| matches!(i, StreamItem::Event(_))),
            "the event after the gap still arrives"
        );
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

    /// Every liveness classification in one table, each row keeping the
    /// exact process shape (and its original comment) it was pinned on.
    #[test]
    fn pane_agent_alive_rows() {
        let cases: [(&str, &str, PaneProcessInfo, Option<bool>); 9] = [
            // Live agent: the agent binary leads the foreground group (pid == fgpgid).
            (
                "live_opencode_classified_alive",
                "opencode",
                info(
                    Some(100),
                    Some(50),
                    vec![proc("opencode", 100), proc("node", 101), proc("uv", 102)],
                ),
                Some(true),
            ),
            // Dead process: pane exists but the shell owns the foreground — the
            // exact bd-1np false-negative shape (pane present, process gone).
            (
                "dead_process_shell_foreground_classified_dead",
                "opencode",
                info(Some(50), Some(50), vec![proc("zsh", 50)]),
                Some(false),
            ),
            // Dead process with orphaned children still listed (MCP servers
            // linger after the agent died): no agent binary, foreground not
            // shell-owned.
            (
                "dead_process_orphan_children_classified_dead",
                "opencode",
                info(
                    Some(103),
                    Some(50),
                    vec![proc("node", 103), proc("uv", 104)],
                ),
                Some(false),
            ),
            // Empty foreground list: nothing running.
            (
                "empty_foreground_classified_dead",
                "opencode",
                info(Some(50), Some(50), vec![]),
                Some(false),
            ),
            // Unknown kind with no other signal: inconclusive, never "healthy".
            (
                "unknown_kind_inconclusive",
                "mystery-kind",
                info(Some(200), Some(50), vec![proc("mystery-agent", 200)]),
                None,
            ),
            // Unknown kind but shell owns the foreground: still provably dead.
            (
                "unknown_kind_shell_foreground_dead",
                "mystery-kind",
                info(Some(50), Some(50), vec![proc("zsh", 50)]),
                Some(false),
            ),
            // Kind aliases: grok's binary can be grok-build.
            (
                "kind_alias_matches",
                "grok",
                info(Some(300), Some(50), vec![proc("grok-build", 300)]),
                Some(true),
            ),
            // bd-oc-launch-chdir-enoent-1l30: npm/mise-installed CLIs bin
            // opencode as `opencode.exe` (measured live 2026-10-07: mise
            // npm-opencode-cli 2.0.24, process name `opencode.exe`), so an
            // exact-name match read every LIVE oc pane as a dead shell —
            // `add --oc 1` bailed "agent process gone" while the TUI sat
            // interactive in the pane.
            (
                "live_opencode_npm_exe_suffix_classified_alive",
                "opencode",
                info(
                    Some(100),
                    Some(50),
                    vec![proc("opencode.exe", 100), proc("node", 101)],
                ),
                Some(true),
            ),
            // Argv0-only match (name may be a comm name like python3.14t).
            (
                "argv0_match_counts",
                "opencode",
                info(
                    Some(400),
                    Some(50),
                    vec![PaneProcessInfoProcess {
                        pid: 400,
                        name: "opencode".to_string(),
                        argv0: Some("/opt/bin/opencode".to_string()),
                    }],
                ),
                Some(true),
            ),
        ];
        for (case, kind, i, want) in cases {
            assert_eq!(pane_agent_alive(&i, kind), want, "{case}");
        }
    }

    #[test]
    fn kind_names_mapping_has_all_four() {
        assert!(kind_process_names("opencode").contains(&"opencode"));
        assert!(kind_process_names("pi").contains(&"pi"));
        assert!(kind_process_names("grok").contains(&"grok"));
        assert!(kind_process_names("pi").contains(&"pi"));
    }
}

#[cfg(test)]
mod herdr_env_tests {
    use super::{apply_opencode_go_api_key_alias, herdr_integration_env};

    fn cfg() -> crate::config::Config {
        crate::config::Config::default()
    }

    /// bd-pin-public-plane-s-easy.2: the pane env carries the agent's mail
    /// PIN as AGENT_NAME + FLYWHEEL_MAIL_AS — the identity the spawned agent
    /// boots knowing it is addressed by (same pin the peer template injects
    /// and the loop substitutes for {AGENT_NAME}).
    #[test]
    fn env_carries_pin_as_agent_name_and_mail_as() {
        let env = herdr_integration_env(&cfg(), Some("DustySparrow"));
        assert_eq!(env.get("HERDR_ENV").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("AGENT_NAME").map(String::as_str),
            Some("DustySparrow")
        );
        assert_eq!(
            env.get("FLYWHEEL_MAIL_AS").map(String::as_str),
            Some("DustySparrow")
        );
    }

    #[test]
    fn env_without_pin_omits_identity_keys() {
        let env = herdr_integration_env(&cfg(), None);
        assert!(env.contains_key("HERDR_ENV"));
        assert!(!env.contains_key("AGENT_NAME"));
        assert!(!env.contains_key("FLYWHEEL_MAIL_AS"));
    }

    #[test]
    fn env_ignores_empty_pin() {
        let env = herdr_integration_env(&cfg(), Some(""));
        assert!(!env.contains_key("AGENT_NAME"));
        assert!(!env.contains_key("FLYWHEEL_MAIL_AS"));
    }

    /// GS-037 P0-4: OPENCODE_GO_API_KEY aliases into OPENCODE_API_KEY when API unset.
    #[test]
    fn env_aliases_opencode_go_api_key_when_api_unset() {
        let _g = crate::config::lock_env();
        unsafe {
            std::env::remove_var("OPENCODE_API_KEY");
            std::env::set_var("OPENCODE_GO_API_KEY", "test-go-key-not-a-secret-fixture");
        }
        let env = herdr_integration_env(&cfg(), None);
        assert_eq!(
            env.get("OPENCODE_API_KEY").map(String::as_str),
            Some("test-go-key-not-a-secret-fixture")
        );
        unsafe {
            std::env::remove_var("OPENCODE_GO_API_KEY");
            std::env::remove_var("OPENCODE_API_KEY");
        }
    }

    #[test]
    fn env_does_not_clobber_existing_opencode_api_key() {
        let _g = crate::config::lock_env();
        unsafe {
            std::env::set_var("OPENCODE_API_KEY", "already-set");
            std::env::set_var("OPENCODE_GO_API_KEY", "go-key");
        }
        let mut map = std::collections::BTreeMap::new();
        apply_opencode_go_api_key_alias(&mut map);
        assert!(
            !map.contains_key("OPENCODE_API_KEY"),
            "must not invent a pane override when process already has API key"
        );
        unsafe {
            std::env::remove_var("OPENCODE_GO_API_KEY");
            std::env::remove_var("OPENCODE_API_KEY");
        }
    }
}

#[cfg(test)]
mod sanitize_pane_text_tests {
    use super::sanitize_pane_text;

    /// One row per strip contract — CSI color/cursor/erase, OSC title,
    /// one-shot escapes, glyph families, and the plain/empty/unterminated
    /// edges. Same bytes in, same bytes out as the nine tests this replaces;
    /// the row name is what a failure reports.
    #[test]
    fn sanitize_pane_text_rows() {
        let rows = [
            (
                "csi-color",
                "\u{1b}[31mred\u{1b}[0m and \u{1b}[1;32mbold green\u{1b}[m",
                "red and bold green",
            ),
            ("csi-cursor-erase", "a\u{1b}[2Kb\u{1b}[1;1Hc", "abc"),
            (
                "osc-title-bel",
                "before\u{1b}]0;term-title\u{07}after",
                "beforeafter",
            ),
            (
                "one-shot-escapes",
                "abc\u{1b}7def\u{1b}8ghi\u{1b}=j",
                "abcdefghij",
            ),
            ("double-line-heavy-box", "╔═══╗ ║ ╠ ╣ ╦ ╩ ╬", "      "),
            ("block-element-shade", "▀▄█░▒▓text", "text"),
            ("plain-unchanged", "hello world", "hello world"),
            ("empty-unchanged", "", ""),
            ("unterminated-csi", "lead\u{1b}[31m", "lead"),
        ];
        for (name, input, expected) in rows {
            assert_eq!(sanitize_pane_text(input), expected, "row {name}");
        }
    }

    #[test]
    fn strips_osc_hyperlink_with_st_terminator() {
        assert_eq!(
            sanitize_pane_text("\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\more"),
            "linkmore"
        );
    }

    #[test]
    fn strips_box_drawing_border_glyphs() {
        let border = "┏━━━━━━┓\n┃ ok   ┃\n┗━━━━━━┛";
        assert_eq!(sanitize_pane_text(border), "\n ok   \n");
    }

    #[test]
    fn mixed_garbled_tui_output_becomes_readable() {
        let raw = "\u{1b}[2J\u{1b}[H╔═╗ \u{1b}[1;32mREADY\u{1b}[0m ═╝\u{1b}[?25l";
        assert_eq!(sanitize_pane_text(raw), " READY ");
    }
}

#[cfg(test)]
mod responsiveness_tests {
    use super::{Responsiveness, responsiveness_verdict};

    /// Every verdict shape in one table: advanced seq across three windows,
    /// the bd-a8i frozen-idle respawn, busy-frozen, sub-second window, and a
    /// seq regression.
    #[test]
    fn responsiveness_verdict_rows() {
        let cases: [(&str, u64, u64, u64, bool, Responsiveness); 7] = [
            // Seq advanced across the window: state changed, so the agent is
            // responsive whatever the idle status or window length.
            (
                "advanced_seq_responsive_3s",
                10,
                11,
                3,
                true,
                Responsiveness::Responsive,
            ),
            (
                "advanced_seq_responsive_zero_window",
                10,
                11,
                0,
                true,
                Responsiveness::Responsive,
            ),
            (
                "advanced_seq_responsive_not_idle",
                10,
                42,
                3,
                false,
                Responsiveness::Responsive,
            ),
            // The bd-a8i shape: seq frozen + Idle across a full window — alive
            // as a PID, unresponsive to input, needs respawn.
            (
                "frozen_seq_idle_unresponsive",
                10,
                10,
                3,
                true,
                Responsiveness::Unresponsive,
            ),
            // Frozen seq but the agent reports non-idle: Working/Blocked agents
            // may legitimately hold the counter — never respawn on ambiguous
            // evidence.
            (
                "frozen_seq_busy_inconclusive",
                10,
                10,
                3,
                false,
                Responsiveness::Inconclusive,
            ),
            // Sub-second window: not enough time to observe a change — a frozen
            // agent must not be declared from a near-zero window.
            (
                "zero_window_inconclusive",
                10,
                10,
                0,
                true,
                Responsiveness::Inconclusive,
            ),
            // Seq regression: the per-pane counter is monotonic; a regression
            // means the samples are unreliable, so the verdict is inconclusive.
            (
                "regressed_seq_inconclusive",
                10,
                9,
                3,
                true,
                Responsiveness::Inconclusive,
            ),
        ];
        for (case, prev_seq, curr_seq, window_secs, idle, want) in cases {
            assert_eq!(
                responsiveness_verdict(prev_seq, curr_seq, window_secs, idle),
                want,
                "{case}"
            );
        }
    }
}

// ---- bd-eign.5: structured opencode session output ----
//
// `flywheel output` reads an agent's ACTUAL session from the opencode
// structured store (SQLite, `~/.local/share/opencode/opencode.db`) instead of
// TUI scrollback cells — the alternate screen renders garbled artifacts even
// after sanitize_pane_text. Parts carry text/tool/reasoning payloads with the
// real tool inputs and outputs.

use std::path::{Path, PathBuf};

/// The opencode session store path. `OPENCODE_DB` env override, else
/// `~/.local/share/opencode/opencode.db` (XDG_DATA_HOME aware).
fn opencode_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("OPENCODE_DB") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local/share")
        });
    base.join("opencode").join("opencode.db")
}

/// Run a SQL query against the opencode store with `-json` (SQLite CLI,
/// always present on dev hosts). Empty result rows → empty JSON array
/// (sqlite3 prints nothing). The SQL is built from local trusted inputs
/// (paths/names); single quotes are doubled anyway.
fn sqlite_json(db: &Path, sql: &str) -> Result<Value> {
    let out = std::process::Command::new("sqlite3")
        .arg("-json")
        .arg(db)
        .arg(sql)
        .output()
        .with_context(|| format!("run sqlite3 against {}", db.display()))?;
    if !out.status.success() {
        bail!(
            "sqlite3 query failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if out.stdout.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    serde_json::from_slice(&out.stdout).context("parse sqlite3 -json output")
}

fn sq(s: &str) -> String {
    s.replace('\'', "''")
}

/// Cap a raw payload field at `max` chars with a marker (tool I/O and
/// reasoning traces can run to tens of KB — the CLI view needs the tail
/// shape, not the full dump).
fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n…(truncated)");
    out
}

/// Tool-part state summary: status + capped input/output. The tool OUTPUT is
/// the key observability win over scrollback.
fn summarize_tool_state(state: &Value) -> Value {
    let status = state.get("status").and_then(Value::as_str).unwrap_or("?");
    let input = state
        .get("input")
        .map(|v| {
            v.as_str()
                .map(|s| cap(s, 2000))
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default();
    let output = state
        .get("output")
        .map(|v| {
            v.as_str()
                .map(|s| cap(s, 4000))
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default();
    json!({
        "status": status,
        "input": input,
        "output": output,
    })
}

/// Summarize one opencode `part.data` payload into a viewable record.
/// Internal parts (snapshots, step-start) return None — they carry no
/// user-visible content.
fn summarize_part(data: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(data).ok()?;
    let kind = v.get("type").and_then(Value::as_str)?;
    match kind {
        "text" => v
            .get("text")
            .and_then(Value::as_str)
            .map(|t| json!({"kind": "text", "text": t})),
        "reasoning" => v
            .get("text")
            .and_then(Value::as_str)
            .map(|t| json!({"kind": "reasoning", "text": cap(t, 2000)})),
        "tool" => Some(json!({
            "kind": "tool",
            "tool": v.get("tool").and_then(Value::as_str).unwrap_or("?"),
            "state": summarize_tool_state(v.get("state").unwrap_or(&Value::Null)),
        })),
        _ => None,
    }
}

/// bd-eign.5: read an agent's structured output from the opencode session
/// store. Resolution: exact `session_id` wins; else the LATEST session whose
/// `directory` matches `project_dir` (optionally narrowed to an opencode role
/// like `build`/`explore`). Returns the session meta + summarized parts.
/// No TUI scrollback — the alternate-screen garble problem disappears.
pub fn read_session_log(
    project_dir: &str,
    role: Option<&str>,
    session_id: Option<&str>,
    limit: u32,
) -> Result<Value> {
    let db = opencode_db_path();
    if !db.exists() {
        bail!(
            "opencode session store not found: {} (run opencode first)",
            db.display()
        );
    }
    let (sid, meta): (String, Value) = if let Some(sid) = session_id {
        let rows = sqlite_json(
            &db,
            &format!(
                "SELECT id, title, agent, model, time_updated FROM session WHERE id='{}'",
                sq(sid)
            ),
        )?;
        let arr = rows.as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            bail!("no opencode session {sid}");
        }
        (sid.to_string(), Value::Array(arr))
    } else {
        let agent_clause = role
            .map(|r| format!(" AND agent='{}'", sq(r)))
            .unwrap_or_default();
        let sql = format!(
            "SELECT id, title, agent, model, time_updated FROM session \
             WHERE directory='{}'{agent_clause} ORDER BY time_updated DESC LIMIT 1",
            sq(project_dir)
        );
        let rows = sqlite_json(&db, &sql)?;
        let arr = rows.as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            bail!(
                "no opencode session for directory {project_dir}{} — run opencode there first",
                role.map(|r| format!(" (agent {r})")).unwrap_or_default()
            );
        }
        let sid = arr[0]["id"].as_str().unwrap_or_default().to_string();
        (sid, Value::Array(arr))
    };
    let parts_sql = format!(
        "SELECT data FROM part WHERE session_id='{}' ORDER BY time_created, id LIMIT {}",
        sq(&sid),
        limit.max(1)
    );
    let raw = sqlite_json(&db, &parts_sql)?;
    let parts: Vec<Value> = raw
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|row| row["data"].as_str().and_then(summarize_part))
        .collect();
    let mut out = json!({
        "session_id": sid,
        "directory": project_dir,
        "parts": parts,
    });
    if let Some(m) = meta.as_array().and_then(|a| a.first()) {
        for key in ["title", "agent", "model", "time_updated"] {
            if let Some(v) = m.get(key) {
                out[key] = v.clone();
            }
        }
    }
    Ok(out)
}

/// First line of `text`, trimmed, capped at 200 chars with an ellipsis.
/// One detail line per action — a tail is for scanning, not for dumping a
/// whole file edit. Pure.
fn one_line_detail(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.chars().count() <= 200 {
        return first.to_string();
    }
    let mut out: String = first.chars().take(200).collect();
    out.push('\u{2026}');
    out
}

/// One legible line for a pi tool call: the argument a human would search
/// for (`command` for bash, `path` for file tools, `query`/`pattern` for
/// search). Unknown argument shapes fall back to the compact JSON so an
/// unmodeled tool still reads. Pure.
fn pi_tail_detail(arguments: &Value) -> String {
    let text = ["command", "path", "query", "pattern", "code", "content"]
        .iter()
        .find_map(|k| arguments.get(*k).and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| arguments.to_string());
    one_line_detail(&text)
}

/// Last `n` actions from a pi session JSONL: every assistant `toolCall`
/// part is one action. Malformed lines are skipped — a live session's last
/// line can be torn mid-write, and that is not a reason to show nothing.
/// Pure.
fn pi_tail_events(raw: &str, n: usize) -> Vec<Value> {
    let mut events: Vec<Value> = Vec::new();
    for line in raw.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(msg) = row.get("message") else {
            continue;
        };
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(parts) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("toolCall") {
                continue;
            }
            events.push(json!({
                "ts": row.get("timestamp").and_then(Value::as_str),
                "kind": "tool",
                "tool": part.get("name").and_then(Value::as_str).unwrap_or("?"),
                "detail": pi_tail_detail(part.get("arguments").unwrap_or(&Value::Null)),
            }));
        }
    }
    if events.len() > n {
        events.split_off(events.len() - n)
    } else {
        events
    }
}

/// Map already-summarized opencode session parts (oldest-first, the
/// [`summarize_part`] shape) to tail events — the newest `n` tool calls.
/// Pure; the SQL fetch lives in `read_agent_session_tail`.
fn opencode_tail_parts(parts: &[Value], n: usize) -> Vec<Value> {
    let mut events: Vec<Value> = parts
        .iter()
        .filter(|p| p.get("kind").and_then(Value::as_str) == Some("tool"))
        .map(|p| {
            let input = p
                .get("state")
                .and_then(|s| s.get("input"))
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({
                // opencode parts carry no per-row timestamp in the summary
                // shape; null reads "not recorded", never a fabricated now.
                "ts": Value::Null,
                "kind": "tool",
                "tool": p.get("tool").and_then(Value::as_str).unwrap_or("?"),
                "detail": one_line_detail(input),
            })
        })
        .collect();
    if events.len() > n {
        events.split_off(events.len() - n)
    } else {
        events
    }
}

/// bd-swarm-cockpit-pczs: the last `n` actions from the session herdr
/// reports for a pane (`agent_session`). `path` sessions (pi JSONL) read the
/// file; `id` sessions (opencode) run one DESC LIMIT query against the
/// structured store. Both produce the same rows: `{ts, kind, tool, detail}`.
/// A missing/unreadable source is a loud error, never a silent empty tail.
pub fn read_agent_session_tail(session: &AgentSessionInfo, n: usize) -> Result<Value> {
    let events = match session.kind {
        AgentSessionRefKind::Path => {
            let raw = std::fs::read_to_string(&session.value)
                .map_err(|e| anyhow!("session {} at {}: {e}", session.agent, session.value))?;
            pi_tail_events(&raw, n)
        }
        AgentSessionRefKind::Id => {
            let db = opencode_db_path();
            if !db.exists() {
                bail!(
                    "opencode session store not found: {} (run opencode first)",
                    db.display()
                );
            }
            // Overscan: text/reasoning parts are dropped below, so fetch more
            // than n rows (bounded) before filtering to the newest tool calls.
            let limit = n.saturating_mul(8).clamp(1, 400);
            let raw = sqlite_json(
                &db,
                &format!(
                    "SELECT data FROM part WHERE session_id='{}' \
                     ORDER BY time_created DESC, id DESC LIMIT {limit}",
                    sq(&session.value)
                ),
            )?;
            let mut parts: Vec<Value> = raw
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|row| row["data"].as_str().and_then(summarize_part))
                .collect();
            parts.reverse(); // SQL walked newest-first
            opencode_tail_parts(&parts, n)
        }
    };
    Ok(json!({
        "agent": session.agent,
        "source": session.source,
        "session": session.value,
        "events": events,
    }))
}

/// H4 (bd-harness-alt-screen-reads): alt-screen transcript read primitives.
/// Two adapters implement this — `ClientAltScreenRead` (cmd_view) and
/// `BackendAltScreenRead` (loop diagnostics) — so the shared ladder drives
/// both surfaces with one tested code path.
pub trait AltScreenReadOps {
    /// Read `source` (recent-unwrapped / visible / ...). May return
    /// `BackendErrorCode::AgentNotIdle` while the agent is working.
    fn read(&mut self, source: &str) -> Result<String, BackendError>;
    /// Bounded wait for the agent to become idle. `Ok(true)` = became idle
    /// within the bound; `Ok(false)` = timed out; `Err` = wait failed.
    fn wait_idle(&mut self) -> Result<bool, BackendError>;
    /// Last resort: prompt the agent to write its recent output to a temp
    /// file, then read the file back (herdr docs' file-path contract).
    fn file_fallback(&mut self) -> Result<String, BackendError>;
}

/// Bounded wait budget for `wait_idle` (herdr `agent.wait` timeout).
pub const ALT_SCREEN_IDLE_WAIT_MS: u64 = 5_000;

/// herdr wire `agent.read` source for unwrapped transcript history (the
/// docs call it `recent-unwrapped`; the protocol enum is
/// `recent_unwrapped` — verified live against herdr
/// 0.8.0-preview.2026-08-04-d78e3d3b5126, 2026-08-15).
pub const RECENT_UNWRAPPED_SOURCE: &str = "recent_unwrapped";

/// H4 (bd-harness-alt-screen-reads): the alt-screen transcript read ladder.
///
/// 1. `recent-unwrapped` (the transcript source — wire name
///    `recent_unwrapped`; herdr auto-scrolls the mouse-scroll interface when
///    the agent is idle).
/// 2. `agent_not_idle` → bounded wait for idle + ONE retry.
/// 3. Still unavailable → `visible` buffer.
/// 4. `visible` unavailable → file fallback (docs' last-resort contract).
///
/// An explicit `visible` request reads the visible buffer directly — never
/// ladders (caller asked for the screen, not history).
pub fn alt_screen_read_ladder(
    ops: &mut dyn AltScreenReadOps,
    source: &str,
) -> Result<String, BackendError> {
    if source == "visible" {
        return ops.read("visible");
    }
    match ops.read(RECENT_UNWRAPPED_SOURCE) {
        Ok(t) => return Ok(t),
        Err(e) if e.code() == &BackendErrorCode::AgentNotIdle => {}
        Err(e) => return Err(e),
    }
    // agent_not_idle: bounded wait + single retry
    match ops.wait_idle() {
        Ok(true) => {
            if let Ok(t) = ops.read(RECENT_UNWRAPPED_SOURCE) {
                return Ok(t);
            }
        }
        Ok(false) => {}
        Err(e) => {
            eprintln!("flywheel: alt-screen wait-for-idle failed: {e:#}");
        }
    }
    // still unavailable → visible buffer
    if let Ok(t) = ops.read("visible") {
        return Ok(t);
    }
    // last resort: ask the agent to write its output to a temp file
    ops.file_fallback()
}

/// H4 (bd-harness-alt-screen-reads): the docs' file-path last resort. Prompt
/// the agent to write its recent output to a temp file and reply with the
/// path; then read the file. `prompt` must block until the agent processes
/// the request (agent.prompt with a wait-until-idle window).
pub fn alt_screen_file_fallback(
    target: &str,
    lines: Option<u32>,
    mut prompt: impl FnMut(&str) -> Result<(), BackendError>,
) -> Result<String, BackendError> {
    let path = std::env::temp_dir().join(format!(
        "flywheel-alt-{}-{}.md",
        target.replace(['/', ':'], "_"),
        std::process::id()
    ));
    let text = format!(
        "Your terminal transcript is on the alternate screen and cannot be read remotely. \
         Write the last {} lines of your current session output to {} as plain text, \
         then reply with the path.",
        lines.unwrap_or(40),
        path.display()
    );
    prompt(&text).map_err(|e| {
        BackendError::new(
            BackendErrorCode::Io,
            format!("alt-screen file fallback prompt to {target} failed: {e}"),
        )
    })?;
    std::fs::read_to_string(&path).map_err(|e| {
        BackendError::new(
            BackendErrorCode::Io,
            format!(
                "alt-screen file fallback: {target} did not write {}: {e}",
                path.display()
            ),
        )
    })
}

/// H4 (bd-harness-alt-screen-reads): adapter over the toron-core herdr
/// `Client` (cmd_view path).
pub struct ClientAltScreenRead<'a> {
    c: &'a mut Client,
    target: &'a str,
    lines: Option<u32>,
}

impl<'a> ClientAltScreenRead<'a> {
    pub fn new(c: &'a mut Client, target: &'a str, lines: Option<u32>) -> Self {
        Self { c, target, lines }
    }
}

impl AltScreenReadOps for ClientAltScreenRead<'_> {
    fn read(&mut self, source: &str) -> Result<String, BackendError> {
        self.c.agent_read(self.target, source, self.lines)
    }
    fn wait_idle(&mut self) -> Result<bool, BackendError> {
        match self.c.agent_wait(
            self.target,
            &[AgentStatus::Idle],
            Some(ALT_SCREEN_IDLE_WAIT_MS),
        ) {
            Ok(_) => Ok(true),
            Err(e) if e.code() == &BackendErrorCode::Timeout => Ok(false),
            Err(e) => Err(e),
        }
    }
    fn file_fallback(&mut self) -> Result<String, BackendError> {
        let target = self.target;
        let lines = self.lines;
        let c = &mut *self.c;
        let mut prompt = |text: &str| {
            c.agent_prompt_coded(
                target,
                text,
                Some(WaitOptions {
                    timeout_ms: Some(ALT_SCREEN_IDLE_WAIT_MS),
                    until: vec![AgentStatus::Idle],
                }),
            )
            .map(|_| ())
        };
        alt_screen_file_fallback(target, lines, &mut prompt)
    }
}

/// H4 (bd-harness-alt-screen-reads): adapter over the unified backend trait
/// (`Box<dyn Backend>`, loop diagnostics path). Reads are `{text}` shaped.
pub struct BackendAltScreenRead<'a> {
    b: &'a dyn crate::backend::MultiplexerBackend,
    target: &'a str,
    lines: u32,
}

impl<'a> BackendAltScreenRead<'a> {
    pub fn new(b: &'a dyn crate::backend::MultiplexerBackend, target: &'a str, lines: u32) -> Self {
        Self { b, target, lines }
    }
}

impl AltScreenReadOps for BackendAltScreenRead<'_> {
    fn read(&mut self, source: &str) -> Result<String, BackendError> {
        let v = self.b.agent_read(self.target, source, self.lines)?;
        v.get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::Malformed,
                    format!("agent.read missing text for {}: {v}", self.target),
                )
            })
    }
    fn wait_idle(&mut self) -> Result<bool, BackendError> {
        match self.b.agent_wait(
            self.target,
            &[AgentStatus::Idle],
            Some(ALT_SCREEN_IDLE_WAIT_MS),
        ) {
            Ok(_) => Ok(true),
            Err(e) if e.code() == &BackendErrorCode::Timeout => Ok(false),
            Err(e) => Err(e),
        }
    }
    fn file_fallback(&mut self) -> Result<String, BackendError> {
        let target = self.target;
        let lines = self.lines;
        let b = self.b;
        let mut prompt = |text: &str| {
            b.prompt_coded(
                target,
                text,
                Some(&WaitOptions {
                    timeout_ms: Some(ALT_SCREEN_IDLE_WAIT_MS),
                    until: vec![AgentStatus::Idle],
                }),
            )
            .map(|_| ())
        };
        alt_screen_file_fallback(target, Some(lines), &mut prompt)
    }
}

#[cfg(test)]
mod alt_screen_read_ladder_tests {
    use super::*;
    use toron_core::backend::error::{BackendError, BackendErrorCode};

    struct MockOps {
        reads: Vec<(String, Result<String, BackendError>)>,
        waits: Vec<Result<bool, BackendError>>,
        fallbacks: Vec<Result<String, BackendError>>,
        log: Vec<String>,
    }

    impl AltScreenReadOps for MockOps {
        fn read(&mut self, source: &str) -> Result<String, BackendError> {
            self.log.push(format!("read:{source}"));
            let (_, res) = self.reads.remove(0);
            res
        }
        fn wait_idle(&mut self) -> Result<bool, BackendError> {
            self.log.push("wait_idle".into());
            self.waits.remove(0)
        }
        fn file_fallback(&mut self) -> Result<String, BackendError> {
            self.log.push("file_fallback".into());
            self.fallbacks.remove(0)
        }
    }

    fn not_idle(msg: &str) -> BackendError {
        BackendError::new(BackendErrorCode::AgentNotIdle, msg)
    }

    fn ok(text: &str) -> Result<String, BackendError> {
        Ok(text.to_string())
    }

    /// H4: an idle agent's recent-unwrapped read succeeds on the FIRST
    /// attempt — no wait, no fallback.
    #[test]
    fn alt_screen_read_ladder_reads_recent_unwrapped_when_idle() {
        let mut m = MockOps {
            reads: vec![("recent_unwrapped".into(), ok("transcript"))],
            waits: vec![],
            fallbacks: vec![],
            log: vec![],
        };
        let out = alt_screen_read_ladder(&mut m, "recent").expect("read");
        assert_eq!(out, "transcript");
        assert_eq!(m.log, vec!["read:recent_unwrapped"]);
    }

    /// H4: agent_not_idle on the first read → bounded wait for idle →
    /// retry recent-unwrapped → success. File fallback never invoked.
    #[test]
    fn alt_screen_read_ladder_waits_and_retries_after_agent_not_idle() {
        let mut m = MockOps {
            reads: vec![
                ("recent_unwrapped".into(), Err(not_idle("working"))),
                ("recent_unwrapped".into(), ok("transcript")),
            ],
            waits: vec![Ok(true)],
            fallbacks: vec![],
            log: vec![],
        };
        let out = alt_screen_read_ladder(&mut m, "recent").expect("read");
        assert_eq!(out, "transcript");
        assert_eq!(
            m.log,
            vec![
                "read:recent_unwrapped",
                "wait_idle",
                "read:recent_unwrapped"
            ]
        );
    }

    /// H4: wait times out (never became idle) → fall to the visible buffer.
    #[test]
    fn alt_screen_read_ladder_visible_fallback_after_wait_timeout() {
        let mut m = MockOps {
            reads: vec![
                ("recent_unwrapped".into(), Err(not_idle("working"))),
                ("visible".into(), ok("visible buffer")),
            ],
            waits: vec![Ok(false)],
            fallbacks: vec![],
            log: vec![],
        };
        let out = alt_screen_read_ladder(&mut m, "recent").expect("read");
        assert_eq!(out, "visible buffer");
        assert_eq!(
            m.log,
            vec!["read:recent_unwrapped", "wait_idle", "read:visible"]
        );
    }

    /// H4: wait succeeds, retry still agent_not_idle → visible → visible
    /// also not idle → file fallback (the docs' last-resort contract).
    #[test]
    fn alt_screen_read_ladder_file_fallback_when_visible_not_idle() {
        let mut m = MockOps {
            reads: vec![
                ("recent_unwrapped".into(), Err(not_idle("working"))),
                ("recent_unwrapped".into(), Err(not_idle("still working"))),
                ("visible".into(), Err(not_idle("alt screen"))),
            ],
            waits: vec![Ok(true)],
            fallbacks: vec![ok("file transcript")],
            log: vec![],
        };
        let out = alt_screen_read_ladder(&mut m, "recent").expect("read");
        assert_eq!(out, "file transcript");
        assert_eq!(
            m.log,
            vec![
                "read:recent_unwrapped",
                "wait_idle",
                "read:recent_unwrapped",
                "read:visible",
                "file_fallback",
            ]
        );
    }

    /// H4: a non-idle error on the first read propagates — no wait, no
    /// fallback (never mask real backend failures).
    #[test]
    fn alt_screen_read_ladder_propagates_hard_read_errors() {
        let hard = BackendError::new(BackendErrorCode::Connect, "socket gone");
        let mut m = MockOps {
            reads: vec![("recent_unwrapped".into(), Err(hard.clone()))],
            waits: vec![],
            fallbacks: vec![],
            log: vec![],
        };
        let err = alt_screen_read_ladder(&mut m, "recent").expect_err("connect error");
        assert_eq!(err.code(), &BackendErrorCode::Connect);
        assert_eq!(m.log, vec!["read:recent_unwrapped"]);
    }

    /// H4: an explicit `visible` request reads the visible buffer directly —
    /// never ladders, never waits.
    #[test]
    fn alt_screen_read_ladder_visible_source_never_ladders() {
        let mut m = MockOps {
            reads: vec![("visible".into(), ok("direct visible"))],
            waits: vec![],
            fallbacks: vec![],
            log: vec![],
        };
        let out = alt_screen_read_ladder(&mut m, "visible").expect("read");
        assert_eq!(out, "direct visible");
        assert_eq!(m.log, vec!["read:visible"]);
    }

    /// H4: when the file fallback itself fails, the error surfaces (with
    /// the agent_not_idle context preserved by the caller's map_err).
    #[test]
    fn alt_screen_read_ladder_file_fallback_error_propagates() {
        let fb_err = BackendError::new(BackendErrorCode::Io, "temp file missing");
        let mut m = MockOps {
            reads: vec![
                ("recent_unwrapped".into(), Err(not_idle("working"))),
                ("recent_unwrapped".into(), Err(not_idle("still working"))),
                ("visible".into(), Err(not_idle("alt screen"))),
            ],
            waits: vec![Ok(false)],
            fallbacks: vec![Err(fb_err.clone())],
            log: vec![],
        };
        let err = alt_screen_read_ladder(&mut m, "recent").expect_err("file fallback fails");
        assert_eq!(err.code(), &BackendErrorCode::Io);
    }
}

#[cfg(test)]
mod session_log_tests {
    use super::{cap, summarize_part};

    #[test]
    fn text_parts_pass_through() {
        let v = summarize_part(r#"{"type":"text","text":"hello agent"}"#).unwrap();
        assert_eq!(v["kind"], "text");
        assert_eq!(v["text"], "hello agent");
    }

    #[test]
    fn reasoning_capped() {
        let long = "r".repeat(5000);
        let v = summarize_part(&format!(r#"{{"type":"reasoning","text":"{long}"}}"#)).unwrap();
        assert_eq!(v["kind"], "reasoning");
        assert_eq!(
            v["text"].as_str().unwrap().len(),
            2000 + "\n…(truncated)".len()
        );
    }

    #[test]
    fn tool_parts_carry_input_and_output() {
        let v = summarize_part(
            r#"{"type":"tool","tool":"bash","state":{"status":"completed","input":"ls","output":"src"}}"#,
        )
        .unwrap();
        assert_eq!(v["kind"], "tool");
        assert_eq!(v["tool"], "bash");
        assert_eq!(v["state"]["status"], "completed");
        assert_eq!(v["state"]["input"], "ls");
        assert_eq!(v["state"]["output"], "src");
    }

    #[test]
    fn tool_output_capped() {
        let long = "o".repeat(9000);
        let v = summarize_part(&format!(
            r#"{{"type":"tool","tool":"bash","state":{{"status":"completed","input":"x","output":"{long}"}}}}"#
        ))
        .unwrap();
        assert_eq!(
            v["state"]["output"].as_str().unwrap().len(),
            4000 + "\n…(truncated)".len()
        );
    }

    #[test]
    fn internal_parts_skipped() {
        assert!(summarize_part(r#"{"type":"step-start"}"#).is_none());
        assert!(summarize_part(r#"{"type":"snapshot","snapshot":"abc"}"#).is_none());
        assert!(summarize_part("not json").is_none());
    }

    #[test]
    fn cap_passthrough_below_max() {
        assert_eq!(cap("short", 10), "short");
        assert_eq!(cap("hello", 3), "hel\n…(truncated)");
    }

    #[test]
    fn tool_without_state_renders_question_mark() {
        let v = summarize_part(r#"{"type":"tool","tool":"edit"}"#).unwrap();
        assert_eq!(v["state"]["status"], "?");
    }
}

/// bd-swarm-cockpit-pczs: pi session JSONL -> recent tool-call actions.
#[cfg(test)]
mod pi_tail_tests {
    use super::{opencode_tail_parts, pi_tail_detail, pi_tail_events, read_agent_session_tail};

    const FIXTURE: &str = concat!(
        r#"{"type":"session","id":"s1","cwd":"/tmp"}"#,
        "\n",
        r#"{"type":"message","timestamp":"2026-10-04T07:40:32.981Z","message":{"role":"user","content":[{"type":"text","text":"go"}]}}"#,
        "\n",
        r#"{"type":"message","timestamp":"2026-10-04T07:40:33.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"toolCall","name":"bash","arguments":{"command":"git status\n# not this line"}}]}}"#,
        "\n",
        r#"{"type":"message","timestamp":"2026-10-04T07:40:48.681Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"edit","arguments":{"path":"src/lib.rs","edits":[]}}]}}"#,
        "\n",
        "not json at all",
        "\n",
        r#"{"type":"message","timestamp":"2026-10-04T07:41:00.000Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"mystery_tool","arguments":{"blob":"x"}}]}}"#,
        "\n",
        r#"{"type":"message","timestamp":"2026-10-04T07:41:01.000Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"where","arguments":{"query":"find me"}}]}}"#,
        "\n",
    );

    #[test]
    fn pi_tail_events_takes_the_last_n_and_skips_non_actions() {
        let events = pi_tail_events(FIXTURE, 10);
        assert_eq!(events.len(), 4, "3 toolCalls + 1 unknown-arg toolCall");
        assert_eq!(events[0]["tool"], "bash");
        assert_eq!(events[0]["detail"], "git status", "first line only");
        assert_eq!(events[0]["ts"], "2026-10-04T07:40:33.000Z");
        assert_eq!(events[1]["tool"], "edit");
        assert_eq!(events[1]["detail"], "src/lib.rs");
        assert_eq!(events[2]["tool"], "mystery_tool");
        assert!(
            events[2]["detail"].as_str().unwrap().contains("\"blob\""),
            "unknown args fall back to compact JSON: {}",
            events[2]["detail"]
        );
        assert_eq!(events[3]["detail"], "find me");
        let last2 = pi_tail_events(FIXTURE, 2);
        assert_eq!(last2.len(), 2);
        assert_eq!(last2[0]["tool"], "mystery_tool", "last n wins");
    }

    #[test]
    fn pi_tail_detail_truncates_and_handles_missing_args() {
        let long = pi_tail_detail(&serde_json::json!({"command": "x".repeat(500)}));
        assert_eq!(long.chars().count(), 201, "200 chars + ellipsis");
        assert!(long.ends_with('\u{2026}'));
        assert_eq!(pi_tail_detail(&serde_json::json!({})), "{}");
        assert_eq!(pi_tail_detail(&serde_json::Value::Null), "null");
    }

    fn path_session(p: &str) -> super::AgentSessionInfo {
        super::AgentSessionInfo {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: super::AgentSessionRefKind::Path,
            value: p.into(),
        }
    }

    #[test]
    fn read_agent_session_tail_reads_a_path_session_and_refuses_missing() {
        let dir = std::env::temp_dir().join(format!("fw-pi-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("session.jsonl");
        std::fs::write(&path, FIXTURE).expect("fixture");
        let out = read_agent_session_tail(&path_session(path.to_str().unwrap()), 2).expect("read");
        assert_eq!(out["agent"], "pi");
        assert_eq!(out["events"].as_array().unwrap().len(), 2);
        assert_eq!(out["events"][1]["tool"], "where");
        let missing = dir.join("missing.jsonl");
        let err = read_agent_session_tail(&path_session(missing.to_str().unwrap()), 5)
            .expect_err("missing file must error");
        assert!(err.to_string().contains("missing.jsonl"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// opencode parts -> tail rows: only tool parts, newest n, one line of
    /// input. Pure — no ambient opencode DB needed.
    #[test]
    fn opencode_tail_parts_maps_tool_parts_and_takes_newest_n() {
        let parts = vec![
            serde_json::json!({"kind":"text","text":"hi"}),
            serde_json::json!({"kind":"tool","tool":"bash","state":{"status":"completed","input":"cargo test -p flywheel\nmore","output":"ok"}}),
            serde_json::json!({"kind":"tool","tool":"edit","state":{"status":"completed","input":"src/main.rs","output":""}}),
        ];
        let events = opencode_tail_parts(&parts, 5);
        assert_eq!(events.len(), 2, "text parts are not actions");
        assert_eq!(events[0]["tool"], "bash");
        assert_eq!(events[0]["detail"], "cargo test -p flywheel");
        assert_eq!(events[0]["ts"], serde_json::Value::Null);
        let one = opencode_tail_parts(&parts, 1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0]["tool"], "edit", "newest one");
    }
}

/// H3 (bd-harness-integration-preflight): `herdr integration status` CLI
/// output parser + install-target mapping. The gate is
/// `cargo test -p flywheel integration_status`.
#[cfg(test)]
mod integration_status_tests {
    use super::{
        INTEGRATION_KINDS, IntegrationStatus, integration_target, parse_integration_status_line,
        parse_integration_status_output,
    };

    /// `current (vN) (/path)` → installed + current, version captured.
    #[test]
    fn integration_status_parser_current() {
        let (kind, st) = parse_integration_status_line(
            "opencode: current (v9) (/Users/bangedorrunt/.config/opencode/plugins/herdr-agent-state.js)",
        )
        .expect("current line parses");
        assert_eq!(kind, "opencode");
        assert!(st.installed);
        assert!(st.current);
        assert_eq!(st.version.as_deref(), Some("v9"));
        assert_eq!(st.expected_version, None);
        assert_eq!(
            st.path.as_deref(),
            Some("/Users/bangedorrunt/.config/opencode/plugins/herdr-agent-state.js")
        );
    }

    /// `outdated (vN < vM) (/path)` → installed, not current, both versions.
    #[test]
    fn integration_status_parser_outdated() {
        let (kind, st) = parse_integration_status_line(
            "hermes: outdated (v3 < v4) (/Users/bangedorrunt/.hermes/plugins/herdr-agent-state/__init__.py)",
        )
        .expect("outdated line parses");
        assert_eq!(kind, "hermes");
        assert!(st.installed);
        assert!(!st.current);
        assert_eq!(st.version.as_deref(), Some("v3"));
        assert_eq!(st.expected_version.as_deref(), Some("v4"));
        assert_eq!(
            st.path.as_deref(),
            Some("/Users/bangedorrunt/.hermes/plugins/herdr-agent-state/__init__.py")
        );
    }

    /// `not installed (/path)` → installed=false, no versions.
    #[test]
    fn integration_status_parser_not_installed() {
        let (kind, st) = parse_integration_status_line(
            "claude: not installed (/Users/bangedorrunt/.claude/hooks/herdr-agent-state.sh)",
        )
        .expect("not-installed line parses");
        assert_eq!(kind, "claude");
        assert!(!st.installed);
        assert!(!st.current);
        assert_eq!(st.version, None);
        assert_eq!(st.expected_version, None);
        assert_eq!(
            st.path.as_deref(),
            Some("/Users/bangedorrunt/.claude/hooks/herdr-agent-state.sh")
        );
    }

    /// A path containing spaces (herdr prints the literal path) must not
    /// break the state extraction — the LAST ` (...) ` group is the path.
    #[test]
    fn integration_status_parser_path_with_spaces() {
        let (_, st) = parse_integration_status_line(
            "kimi: current (v2) (/Users/bangedorrunt/Library/Application Support/kimi/hooks/herdr-agent-state.sh)",
        )
        .expect("spacey path line parses");
        assert!(st.installed && st.current);
        assert_eq!(
            st.path.as_deref(),
            Some("/Users/bangedorrunt/Library/Application Support/kimi/hooks/herdr-agent-state.sh")
        );
    }

    /// Unknown state text = shape drift: never invent a state, treat as
    /// not installed (the path group is still data — keep it).
    #[test]
    fn integration_status_parser_unknown_state_defaults_not_installed() {
        let (kind, st) = parse_integration_status_line("foo: mysterious (v1) (/tmp/x)")
            .expect("unknown-state line still parses kind");
        assert_eq!(kind, "foo");
        assert!(!st.installed);
        assert_eq!(st.path.as_deref(), Some("/tmp/x"));
    }

    /// Empty / malformed lines are skipped (multi-line output parses as a
    /// map; a shape-drifted line must not poison the rest).
    #[test]
    fn integration_status_parser_skips_garbage_lines() {
        assert!(parse_integration_status_line("").is_none());
        assert!(parse_integration_status_line("   ").is_none());
        assert!(parse_integration_status_line("no-colon-line").is_none());
    }

    /// Full-output parse: the real 0.8.0-preview shape, all states present.
    #[test]
    fn integration_status_output_parses_all_kinds() {
        let raw = "\
pi: current (v8) (/Users/bangedorrunt/.pi/agent/extensions/herdr-agent-state.ts)
omp: current (v8) (/Users/bangedorrunt/.omp/agent/extensions/herdr-omp-agent-state.ts)
claude: not installed (/Users/bangedorrunt/.claude/hooks/herdr-agent-state.sh)
hermes: outdated (v3 < v4) (/Users/bangedorrunt/.hermes/plugins/herdr-agent-state/__init__.py)
";
        let map: std::collections::HashMap<_, _> =
            parse_integration_status_output(raw).into_iter().collect();
        assert_eq!(map.len(), 4);
        assert!(map["pi"].installed && map["pi"].current);
        assert!(map["pi"].installed && map["pi"].current);
        assert!(!map["claude"].installed);
        assert!(map["hermes"].installed && !map["hermes"].current);
    }

    /// Every canonical kind maps to a socket target; the underscore twin
    /// resolves too; custom flywheel kinds resolve to None (preflight skips).
    #[test]
    fn integration_status_target_mapping() {
        for k in INTEGRATION_KINDS {
            assert!(integration_target(k).is_some(), "{k} must map to a target");
        }
        assert_eq!(
            integration_target("antigravity-cli"),
            Some("antigravity_cli")
        );
        assert_eq!(integration_target("my-custom-kind"), None);
    }

    /// not_installed() is the fallback the callers use on absent kinds.
    #[test]
    fn integration_status_not_installed_fallback() {
        let st = IntegrationStatus::not_installed();
        assert!(!st.installed && !st.current);
        assert_eq!(st.version, None);
        assert_eq!(st.expected_version, None);
        assert_eq!(st.path, None);
    }
}

/// bd-pattern-peer-triggered: the shell-readiness gate predicate. A fresh
/// split pane is NOT ready until the shell prints its prompt (line editor
/// up, raw mode); typing the ~2.6KB single-quoted peer template into a
/// booting shell (canonical mode, 1024-byte MAX_CANON) splits it mid-quote
/// and zsh parks at `>....`. The predicate recognizes rendered prompts so
/// the spawn path can wait for readiness instead of racing the shell.
#[cfg(test)]
mod shell_prompt_tests {
    use super::shell_prompt_rendered;

    #[test]
    fn empty_or_booting_shell_is_not_ready() {
        assert!(!shell_prompt_rendered(""));
        assert!(!shell_prompt_rendered("   \n  \n"));
        // Startup noise without a rendered prompt.
        assert!(!shell_prompt_rendered(
            "Last login: Mon Aug 18 04:24:39 on ttys001\n"
        ));
        assert!(!shell_prompt_rendered(
            "loading zsh config...\ngit: command not found"
        ));
    }

    #[test]
    fn rendered_prompt_sigils_are_ready() {
        // zsh default + p10k-style, bash, root, fish.
        for prompt in [
            "❯",
            "❯ ",
            "bangedorrunt in 🌐 mikasa in toron main*\n ⇡≡\n❯",
            "%",
            "$",
            "#",
            ">",
            "➜  ~",
        ] {
            assert!(
                shell_prompt_rendered(prompt),
                "rendered prompt {prompt:?} must be ready"
            );
        }
    }

    #[test]
    fn user_host_path_prompts_are_ready() {
        assert!(shell_prompt_rendered("user@host:~$"));
        assert!(shell_prompt_rendered("bob@macbook:~/work/src"));
    }

    /// bd-ws-resolution (2026-08-17): with a foreign workspace sharing the
    /// bare project label, the flywheel-OWNED label must win even when herdr
    /// reports NO agent names (the omp case — the name scan finds nothing
    /// and the old code fell back to list order, dispatching into a foreign
    /// workspace: "no agents in workspace w37").
    #[test]
    fn resolve_workspace_id_prefers_owned_label_over_foreign() {
        use crate::herdr::{AgentInfo, WorkspaceInfo};
        let mk = |id: &str, label: &str| WorkspaceInfo {
            workspace_id: id.into(),
            number: 0,
            label: label.into(),
            focused: false,
            pane_count: 0,
            tab_count: 0,
            active_tab_id: String::new(),
            agent_status: crate::herdr::AgentStatus::Unknown,
            cwd: None,
        };
        let foreign = mk("w37", "toron");
        let owned = mk("w70", "flywheel-toron");
        // No agent names at all (omp panes) — the label must break the tie.
        let no_agents: Vec<AgentInfo> = Vec::new();
        assert_eq!(
            super::resolve_workspace_id(
                &[foreign.clone(), owned.clone()],
                &no_agents,
                "flywheel-toron-"
            ),
            "w70",
            "owned label wins over list order"
        );
        // A foreign FIRST in list order still loses.
        assert_eq!(
            super::resolve_workspace_id(
                &[foreign.clone(), owned.clone()],
                &no_agents,
                "flywheel-toron-"
            ),
            "w70"
        );
        // Single match: unchanged behavior.
        assert_eq!(
            super::resolve_workspace_id(
                std::slice::from_ref(&foreign),
                &no_agents,
                "flywheel-toron-"
            ),
            "w37"
        );
    }

    /// bd-k8jn: a crew spawned under a mail PIN is named by that PIN, so the
    /// prefix scan in [`resolve_workspace_id`] cannot see it and the tie
    /// falls to list order — the STALE room that carries the bare project
    /// label. The registry's panes are the truth.
    #[test]
    fn a_crew_named_by_its_pin_resolves_to_its_registered_workspace() {
        use crate::herdr::{AgentInfo, AgentStatus, WorkspaceInfo};
        let mk = |id: &str, label: &str| WorkspaceInfo {
            workspace_id: id.into(),
            number: 0,
            label: label.into(),
            focused: false,
            pane_count: 0,
            tab_count: 0,
            active_tab_id: String::new(),
            agent_status: AgentStatus::Unknown,
            cwd: None,
        };
        let mk_agent = |ws: &str, pane: &str, name: &str| AgentInfo {
            agent: Some("opencode".into()),
            agent_status: AgentStatus::Unknown,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: true,
            launch_pending: false,
            name: Some(name.into()),
            pane_id: pane.into(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: format!("{ws}:t1"),
            terminal_id: "term".into(),
            terminal_title: Some(String::new()),
            workspace_id: ws.into(),
        };
        // The stale room comes FIRST in list order and carries the bare
        // project label; the live crew sits in a room labelled by the spawn.
        let stale = mk("w23", "toron");
        let live = mk("w7W", "elm-comms-kernel");
        let agents = vec![mk_agent("w7W", "w7W:p1", "blueridge")];
        let panes = vec!["w7W:p1".to_string()];

        assert_eq!(
            super::resolve_workspace_from_registry(&[stale.clone(), live.clone()], &agents, &panes)
                .as_deref(),
            Some("w7W"),
            "the registered pane names the live room, not the stale one"
        );

        // INVERTED: without the registry the tie falls to list order and the
        // stale room wins — which is exactly the bug.
        assert_eq!(
            super::resolve_workspace_id(&[stale.clone(), live.clone()], &agents, "flywheel-toron-"),
            "w23",
            "the prefix scan cannot see a pin-named crew, so list order wins"
        );

        // No registered pane: the helper declines and the caller's existing
        // resolution stands.
        assert_eq!(
            super::resolve_workspace_from_registry(&[stale.clone(), live.clone()], &agents, &[]),
            None
        );
    }
}

#[cfg(test)]
mod gs049_launch_pending_tests {
    use super::*;
    use toron_core::backend::types::AgentStatus;

    fn agent(pending: bool, ready: bool, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            agent: Some("pi".into()),
            agent_status: status,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: ready,
            launch_pending: pending,
            name: Some("Pin".into()),
            pane_id: "p1".into(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term".into(),
            terminal_title: None,
            workspace_id: "ws".into(),
        }
    }

    #[test]
    fn agent_still_booting_clears_stale_ready() {
        assert!(!agent_still_booting(&agent(
            false,
            false,
            AgentStatus::Idle
        )));
        assert!(agent_still_booting(&agent(
            true,
            false,
            AgentStatus::Unknown
        )));
        assert!(!agent_still_booting(&agent(true, true, AgentStatus::Idle)));
    }
}

/// bd-d7zu E2E: the shell-readiness gate, measured against a REAL pane.
///
/// The dispatch regression this bead fixes is a timing race, so a unit test on
/// the pure detector cannot see it. This test splits a real pane, drives the
/// real `wait_shell_prompt` against the real client, and records what the gate
/// actually saw — the elapsed time and the exact last non-blank line it
/// matched on. The recorded text is the reproducible artifact: if the gate ever
/// degrades into a bare sleep, `matched_on` no longer ends in a prompt sigil and
/// this test fails loudly instead of the dispatch silently going flaky again.
///
/// What it pins:
/// 1. a real fresh pane DOES reach a detected prompt inside the budget — the
///    gate is a real signal, not a timeout that always expires;
/// 2. the line it matched on is a genuine prompt, not a PS2 continuation
///    (a continuation line is the mid-quote park that mangles a long launch
///    arg — the exact 2026-08-18 dead-on-spawn this gate exists to prevent).
#[cfg(test)]
mod shell_gate_e2e {
    use super::*;

    #[test]
    #[ignore = "bd-d7zu LIVE probe: needs a live herdr daemon"]
    fn real_pane_reaches_a_detected_prompt() {
        let cfg = crate::Config::load().expect("config");
        let mut c = Client::connect(Some(&cfg.socket_path())).expect("herdr connect");
        let created = c
            .call(
                "workspace.create",
                json!({ "label": "flywheel-shell-gate-e2e", "cwd": "/tmp", "focus": false }),
            )
            .expect("workspace.create");
        let ws = created
            .get("workspace")
            .cloned()
            .unwrap_or_else(|| created.clone());
        let ws_id = ws
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .expect("workspace_id")
            .to_string();
        let root_obj = created
            .get("root_pane")
            .cloned()
            .unwrap_or_else(|| created.clone());
        let root = root_obj
            .get("pane_id")
            .and_then(|v| v.as_str())
            .expect("root pane id")
            .to_string();
        let split = pane_split(&mut c, &root, "down", Some("/tmp"), None).expect("pane_split");
        let pane = split.pane_id.clone();

        let started = std::time::Instant::now();
        let ready = wait_shell_prompt(&mut c, &pane, Duration::from_secs(15));
        let elapsed = started.elapsed();
        let matched_on = pane_recent_text(&mut c, &pane)
            .ok()
            .and_then(|t| {
                t.lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string())
            })
            .unwrap_or_default();

        let _ = c.call("workspace.close", json!({ "workspace_id": ws_id.clone() }));

        // The artifact: what the gate saw, on a real pane.
        println!("gate: ready={ready} elapsed={elapsed:?} matched_on={matched_on:?}");
        assert!(ready, "gate never detected a prompt on a real pane in 15s");
        assert!(
            elapsed < Duration::from_secs(15),
            "gate only returned at the deadline — that is a sleep, not a signal"
        );
        assert!(
            !matched_on.starts_with('>'),
            "gate matched a PS2 continuation line {matched_on:?} — that is the \
             mid-quote park which mangles a long launch arg"
        );
    }
}

#[cfg(test)]
mod bd_crlx_model_pin_tests {
    use super::apply_opencode_model_pin;
    use std::collections::BTreeMap;

    fn env_with(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn pinned(kv: &[(&str, &str)], model: &str) -> BTreeMap<String, String> {
        let mut env = env_with(kv);
        apply_opencode_model_pin(&mut env, model).expect("pin must apply");
        env
    }

    /// The pin IS the model, in the `provider/model` form opencode's parser
    /// accepts. A bare id would be dropped by opencode, so the shape matters.
    #[test]
    fn pin_sets_the_model_in_provider_model_form() {
        let env = pinned(&[], "opencode-go/muse-spark-1.3-contributor");
        let doc: serde_json::Value =
            serde_json::from_str(env.get("OPENCODE_CONFIG_CONTENT").expect("var set")).unwrap();
        assert_eq!(doc["model"], "opencode-go/muse-spark-1.3-contributor");
    }

    /// bd-crlx, the load-bearing one: the operator's other config must SURVIVE.
    /// opencode merges sources, so the pin is a third document, not a
    /// replacement — this row is what would catch a switch to a clobbering
    /// channel (e.g. writing the operator's opencode.json).
    #[test]
    fn pin_preserves_existing_config_content_keys() {
        let env = pinned(
            &[(
                "OPENCODE_CONFIG_CONTENT",
                r#"{"permissions":[{"action":"shell","resource":"*","effect":"deny"}],"username":"alice"}"#,
            )],
            "opencode-go/muse-spark-1.3-contributor",
        );
        let doc: serde_json::Value =
            serde_json::from_str(env.get("OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(doc["model"], "opencode-go/muse-spark-1.3-contributor");
        assert_eq!(doc["username"], "alice", "operator key preserved");
        assert_eq!(
            doc["permissions"][0]["effect"], "deny",
            "operator permissions preserved"
        );
    }

    /// A later pin replaces the earlier MODEL but keeps everything else — the
    /// merge must be idempotent in the model key, not append a duplicate.
    #[test]
    fn a_second_pin_replaces_the_model_without_duplicating_it() {
        let mut env = pinned(&[], "deepseek/deepseek-flash");
        apply_opencode_model_pin(&mut env, "opencode-go/muse-spark-1.3-contributor").unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(env.get("OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(doc["model"], "opencode-go/muse-spark-1.3-contributor");
        assert_eq!(
            doc.as_object().unwrap().len(),
            1,
            "exactly one model key: {doc}"
        );
    }

    /// A bare model id has no provider half. opencode drops such a reference
    /// as unsupported, which would leave the seat silently unpinned — so the
    /// pin refuses instead (fail-closed).
    #[test]
    fn a_bare_model_id_is_refused_rather_than_silently_dropped() {
        let mut env = BTreeMap::new();
        let err = apply_opencode_model_pin(&mut env, "gpt-5.2").expect_err("must refuse");
        assert!(
            err.to_string().contains("provider/model"),
            "names the required form: {err}"
        );
        assert!(
            !env.contains_key("OPENCODE_CONFIG_CONTENT"),
            "a refused pin must not write a half-configured document"
        );
    }

    /// Unparseable pre-existing content is an ERROR, never an overwrite. The
    /// operator's bytes are not ours to discard.
    #[test]
    fn unparseable_existing_content_is_refused_not_overwritten() {
        let mut env = env_with(&[("OPENCODE_CONFIG_CONTENT", "{not json")]);
        let err =
            apply_opencode_model_pin(&mut env, "deepseek/deepseek-flash").expect_err("must refuse");
        assert!(err.to_string().contains("OPENCODE_CONFIG_CONTENT"), "{err}");
        assert_eq!(
            env.get("OPENCODE_CONFIG_CONTENT").map(String::as_str),
            Some("{not json"),
            "the operator's bytes must be left exactly as they were"
        );
    }

    /// A non-object document is also refused: merging a model key into an
    /// array or a bare string would produce something opencode rejects.
    #[test]
    fn non_object_existing_content_is_refused() {
        let mut env = env_with(&[("OPENCODE_CONFIG_CONTENT", "[1,2,3]")]);
        assert!(apply_opencode_model_pin(&mut env, "deepseek/deepseek-flash").is_err());
        assert_eq!(
            env.get("OPENCODE_CONFIG_CONTENT").map(String::as_str),
            Some("[1,2,3]")
        );
    }

    /// Empty/whitespace existing content is treated as absent, not as a parse
    /// failure — an exported-but-empty shell var must not wedge every pane.
    #[test]
    fn empty_existing_content_is_treated_as_absent() {
        let mut env = env_with(&[("OPENCODE_CONFIG_CONTENT", "   ")]);
        apply_opencode_model_pin(&mut env, "deepseek/deepseek-flash")
            .expect("blank content must not wedge the pane");
        let doc: serde_json::Value =
            serde_json::from_str(env.get("OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(doc["model"], "deepseek/deepseek-flash");
    }
}
