//! Flywheel hc helpers (ex-`hc` shim). ClientExt is gone: `Client` is a
//! newtype over `toron_plane::backend::hc::Client` with inherent adapters for
//! legacy call shapes, plus event-payload decoding and pane helpers.
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

use toron_plane::backend::EventStream as _;
pub use toron_plane::backend::error::{BackendError, BackendErrorCode};
use toron_plane::backend::hc::Client as HcClient;
/// Was HDR_PROTOCOL on hc; hc advertises generation via HC_GENERATION.
pub use toron_plane::backend::hc::HC_GENERATION as HDR_PROTOCOL;
pub use toron_plane::backend::types::{
    AgentInfo, AgentSessionInfo, AgentSessionRefKind, AgentStatus, PaneInfo, SendVerdict,
    Subscription, WaitOptions, WorkspaceInfo, classify_send_result,
};

/// Flywheel-facing hc client. Deref to toron `hc::Client` for native methods;
/// inherent methods below replace the deleted ClientExt bridge for signatures
/// flywheel still calls that hc does not expose 1:1 (workspace_*, agent_prompt, …).
#[derive(Debug)]
pub struct Client {
    inner: HcClient,
}

impl std::ops::Deref for Client {
    type Target = HcClient;
    fn deref(&self) -> &HcClient {
        &self.inner
    }
}

impl std::ops::DerefMut for Client {
    fn deref_mut(&mut self) -> &mut HcClient {
        &mut self.inner
    }
}

impl Client {
    pub fn connect(socket_path: Option<&str>) -> Result<Self, BackendError> {
        Ok(Self {
            inner: HcClient::connect(socket_path)?,
        })
    }
}

/// GS-049 / bd-a2bw: hc `launch_pending` is stale when the pane is already
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

/// bd-bug-omp-hc-wedge: the HC_* launch env an hc-launched
/// harness needs to activate its hc extension (omp's
/// hc-omp-agent-state.ts gates on HC_SOCKET_PATH + pane/terminal id).
/// The agent runs as the shell's child and inherits pane env, so the
/// caller injects at terminal creation: `terminal.run` accepts env + cwd.
/// hc fills the per-terminal ids (HC_TERMINAL_ID/...) itself at run; we
/// supply the two workspace-level vars.
///
/// bd-pin-public-plane-s-easy.2: the pane env additionally carries the
/// agent's mail PIN (`AGENT_NAME` + `FLYWHEEL_MAIL_AS`) so a spawned agent
/// boots knowing the identity it is addressed by — same pin the peer
/// template injects and the loop's `{AGENT_NAME}` substitutes.
pub fn plane_launch_env(
    cfg: &crate::config::Config,
    pin: Option<&str>,
) -> std::collections::BTreeMap<String, String> {
    let session = std::env::var("HC_SESSION").unwrap_or_else(|_| "default".into());
    let mut env = std::collections::BTreeMap::from([
        ("HC_SOCKET_PATH".to_string(), cfg.socket_path()),
        ("HC_SESSION".to_string(), session),
    ]);
    if let Some(pin) = pin.filter(|p| !p.is_empty()) {
        env.insert("AGENT_NAME".to_string(), pin.to_string());
        env.insert("FLYWHEEL_MAIL_AS".to_string(), pin.to_string());
    }
    // The pane is spawned by the running rex server, whose env is the
    // operator's. Anything the agent or its doorbell host writes must land in
    // the SAME roots the spawner resolved, or a sandboxed run (e2e-live) leaks
    // into the live state (bd-yd74j). Only set vars are carried; a var absent
    // here is not invented.
    for key in [
        "HOME",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "TORON_DATA_DIR",
        "TORON_MCP_URL",
        "TORON_MCP_DATA_DIR",
    ] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            env.insert(key.to_string(), value);
        }
    }
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
        if !env.contains_key("OPENCODE_GO_API_KEY")
            && let Ok(go) = std::env::var("OPENCODE_GO_API_KEY")
        {
            env.insert("OPENCODE_GO_API_KEY".to_string(), go);
        }
    }
}

/// bd-crlx: pin an opencode TUI agent's model for one pane.
///
/// opencode v2's ROOT command (the one the opencode harness launch runs)
/// has no `--model` flag — the flag exists only on `opencode run`.
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

/// Parse `AgentStatus` from its wire string (the old inherent `parse` —
/// toron-core exposes `FromStr` instead, so this is a thin free wrapper).
pub fn parse_agent_status(s: &str) -> Result<AgentStatus> {
    s.parse().map_err(|e| anyhow!("{e}"))
}

/// Parse the protocol version from `hc schema --json` output (the
/// top-level `protocol` field). `None` on any failure — the caller decides
/// how to surface an unreadable schema.
pub fn parse_protocol(raw: &str) -> Option<u64> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("protocol").and_then(Value::as_u64))
}

/// A harness kind's integration install state. hc 0.1.18 (tested) exposes no
/// integration surface, so every live status is `not_installed` — the H3
/// preflight warns (with the manual remedy) and never fails the spawn.
/// `flywheel status` / `flywheel deps` surface this state per kind.
///
/// H3 (bd-harness-integration-preflight): spawn preflights each launched
/// kind's integration and installs it when missing/outdated; `flywheel
/// status` / `flywheel deps` surface this state per kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationStatus {
    /// integration file exists at the documented path
    pub installed: bool,
    /// installed version equals the version hc ships (vN == expected)
    pub current: bool,
    /// installed version (`vN`) when known; `None` when not installed
    pub version: Option<String>,
    /// expected version (`vM`) for `outdated` lines; `None` otherwise
    pub expected_version: Option<String>,
    /// install path (the trailing `(...)` group); `None` when unparseable
    pub path: Option<String>,
}

impl IntegrationStatus {
    /// The hc default for any kind with no installed integration.
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

/// Harness kinds flywheel can spawn that historically shipped an hc
/// integration (pre-0.1.18 `hc integration install` surface). Flywheel
/// custom kinds without one are NOT in this list — preflight skips them
/// (nothing to install, nothing to report).
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

/// Map a flywheel kind to its historical integration target name. `None`
/// when the kind has no hc integration. The only spelling difference:
/// `antigravity-cli` (display) vs `antigravity_cli` (old socket enum).
pub fn integration_target(kind: &str) -> Option<&'static str> {
    if kind == "antigravity-cli" {
        return Some("antigravity_cli");
    }
    INTEGRATION_KINDS.iter().copied().find(|k| *k == kind)
}

/// hc 0.1.18 (tested) exposes no integration surface (verified: `hc --help` has no
/// `integration` subcommand and the schema lists no `integration.*` method),
/// so there is nothing to report — every kind reads `not_installed` and the
/// H3 preflight warns with the manual remedy. The map shape is kept (not
/// deleted) because `spawn`/`status`/`deps` share it; the one-line note keeps
/// the degraded preflight visible instead of silently green.
pub fn integration_status_all() -> Result<Vec<(String, IntegrationStatus)>> {
    eprintln!(
        "flywheel: note: hc 0.1.18 (tested) has no integration surface — integration preflight degraded (all kinds read not-installed)"
    );
    Ok(Vec::new())
}

/// Install a kind's hc integration: hc 0.1.18 (tested) has none (no `integration`
/// subcommand, no `integration.*` method), so this always fails loud with
/// the manual remedy. The CALLER (H3 preflight) warns and never fails the
/// spawn — do not invent a CLI subcommand here.
pub fn integration_install_socket(_c: &Client, kind: &str) -> Result<Vec<String>> {
    let _ = integration_target(kind)
        .ok_or_else(|| anyhow!("kind {kind} has no hc integration target"))?;
    bail!(
        "hc 0.1.18 (tested) exposes no integration install for {kind} — lifecycle-state reporting runs \
         on process-based heuristics until the harness reports on its own"
    )
}

/// Strip terminal control sequences and TUI box-drawing chrome from a pane
/// read (bd-3hd).
///
/// hc's `strip_ansi` (sent on every `agent.read`) only removes CSI color
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
/// A spawned crew's terminal name is its MAIL PIN (the spawn names agents by the
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

/// push notification payload wrapper (`kind` names the producer, `data`
/// is the typed payload). Built at the toron-core `BackendEvent` edge by
/// `push_from_backend` — the bin matches on the typed `data` variant.
// `kind` is part of the push envelope; consumers route on `data`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PushEvent {
    pub kind: TerminalEventKind,
    pub data: TerminalEventData,
}

/// The live push kinds: hc 0.1.18 (tested) has no pane/workspace hierarchy and no
/// snake_case server events — toron-core's `BackendEvent` is the whole
/// push surface, and only status + exit carry routing facts. Anything else
/// a future backend emits is dropped at `push_from_backend` (the consumers
/// ignore it today via wildcard arms, so the drop changes no behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalEventKind {
    PaneAgentStatusChangedSub,
    PaneExited,
}

/// flywheel-shaped worktree record. toron-core's `WorktreeInfo` models the
// serde wire shape: parsed for completeness, not every field is read.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
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

/// A terminal ended. Built from toron-core's `BackendEvent::PaneExited` —
/// the only exit signal the doorbell routes on.
// serde wire shape: the exit signal is routed by pane, not by these ids.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct PaneExited {
    pub pane_id: String,
    pub workspace_id: String,
}

/// An agent's status changed. Built from toron-core's
/// `BackendEvent::AgentStatusChanged` (the `Sub` variant is the subscribe
/// push; the bare variant is what the watch loop's `match` names — both
/// carry this payload).
// serde wire shape: status + pane are read; the rest are optional metadata.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
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

/// Typed payload of a `PushEvent`: the live pushes only. The bare
/// `PaneAgentStatusChanged` variant is what the watch loop + doorbell match
/// arms name (the subscription push lands as `..Sub` with the same payload).
#[derive(Debug, Clone)]
// The bare variant is matched by the watch loop even though today only the
// `Sub` form is constructed; removing it would break that arm.
#[allow(dead_code, clippy::enum_variant_names)]
pub enum TerminalEventData {
    PaneAgentStatusChanged(PaneAgentStatusChanged),
    PaneAgentStatusChangedSub(PaneAgentStatusChanged),
    PaneExited(PaneExited),
}

/// Typed plane protocol error carrying the machine-readable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for PlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plane error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for PlaneError {}

/// Map a toron-core `BackendError` onto the flywheel-local typed error.
/// Classification stays on the machine code (`BackendErrorCode::as_str`).
fn to_plane_error(e: toron_plane::backend::error::BackendError) -> PlaneError {
    PlaneError {
        code: e.code().as_str().to_string(),
        message: e.message.clone(),
    }
}

/// Result of a pane output wait.
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

/// Request params for a `terminal.wait` output wait.
///
/// `mode` mirrors the CLI's `--match`/`--regex`, but hc matches plain text
/// only: `"match"` passes the pattern through, `"regex"` is refused loud
/// (a regex passed as plain text would silently never match). `timeout_ms`
/// becomes seconds (the wire unit); `None` omits it (server default 180s).
/// Anything else is a caller bug — bail with code `"usage"`.
pub fn wait_for_output_params(
    pane_id: &str,
    mode: &str,
    pattern: &str,
    timeout_ms: Option<u64>,
) -> std::result::Result<Value, PlaneError> {
    if mode != "match" {
        return Err(PlaneError {
            code: "usage".into(),
            message: format!(
                "unknown pane wait-output mode: {mode} (hc terminal.wait matches plain text only; \
                 pass \"match\" or drop --regex)"
            ),
        });
    }
    let mut params = serde_json::Map::new();
    params.insert(
        "terminal".into(),
        Value::String(toron_plane::backend::hc::strip_id_prefix(pane_id).to_string()),
    );
    params.insert("output".into(), Value::String(pattern.to_string()));
    if let Some(ms) = timeout_ms {
        params.insert("timeout".into(), json!(ms as f64 / 1000.0));
    }
    Ok(Value::Object(params))
}

/// Map a `terminal.wait` result onto the wait outcome. `reason == "output"`
/// is the match; `"timeout"` is the check-in (miss → code `"timeout"`);
/// `"exit"`/`"status"` mean the program ended first (code `"ended"`).
/// Any other reason is shape drift (code `"parse"`). The matched text is
/// read back with `terminal.read` — the wait result carries no snapshot.
pub fn parse_wait_for_output_result(
    res: &Value,
) -> std::result::Result<PaneWaitResult, PlaneError> {
    let parse_err = |msg: String| PlaneError {
        code: "parse".into(),
        message: msg,
    };
    let reason = res
        .get("reason")
        .and_then(Value::as_str)
        .ok_or_else(|| parse_err(format!("terminal.wait result missing reason: {res}")))?;
    let pane_id = res
        .get("terminal")
        .and_then(Value::as_str)
        .map(toron_plane::backend::hc::prefix_id)
        .ok_or_else(|| parse_err(format!("terminal.wait result missing terminal: {res}")))?;
    match reason {
        "output" => Ok(PaneWaitResult {
            pane_id,
            matched_line: None,
            read: res
                .get("read")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default(),
        }),
        "timeout" => Err(PlaneError {
            code: "timeout".into(),
            message: format!("no output match in {pane_id} within timeout"),
        }),
        "exit" | "status" => Err(PlaneError {
            code: "ended".into(),
            message: format!("terminal {pane_id} ended before any output match (reason: {reason})"),
        }),
        other => Err(parse_err(format!(
            "unexpected terminal.wait reason {other:?}: {res}"
        ))),
    }
}

/// A new shell terminal via `terminal.run` (hc has no splits, only
/// top-level terminals; `direction` is ignored). No `argv`: the server runs
/// its own shell. `parent` keeps the tree so the workspace shell owns its
/// panes. Returns the terminal as `PaneInfo` (looked up from `pane_list`
/// so the shape carries the daemon's own folded status, not a re-fold).
pub fn pane_split(
    c: &Client,
    pane_id: &str,
    direction: &str,
    cwd: Option<&str>,
    env: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<PaneInfo> {
    let _ = direction;
    let mut params = serde_json::Map::new();
    params.insert(
        "parent".into(),
        serde_json::json!(toron_plane::backend::hc::strip_id_prefix(pane_id)),
    );
    if let Some(cwd) = cwd {
        params.insert("cwd".into(), serde_json::json!(cwd));
    }
    if let Some(env) = env {
        let mut env = env.clone();
        env.entry("FLYWHEEL_ROLE".to_string())
            .or_insert_with(|| "crew".to_string());
        params.insert("env".into(), serde_json::json!(env));
    }
    params.insert("outlive_parent".into(), serde_json::json!(true));
    let res: toron_plane::backend::hc::Terminal =
        serde_json::from_value(c.call("terminal.run", serde_json::Value::Object(params))?)
            .context("parse terminal.run result")?;
    let want = toron_plane::backend::hc::prefix_id(&res.address);
    c.pane_list()
        .map_err(|e| anyhow!("{e}"))?
        .into_iter()
        .find(|p| p.pane_id == want)
        .ok_or_else(|| anyhow!("terminal.run created {want} but pane_list cannot see it"))
}

/// Wait for `pattern` in a pane's output via `terminal.wait` (plain-text
/// `output` match). Returns a typed `PlaneError` so callers can classify
/// (`timeout` code on a miss, `ended` when the program exits first).
/// `regex` mode is refused loud: hc's output match is plain text only, and
/// a regex passed as plain text would silently never match.
pub fn pane_wait_output(
    c: &Client,
    pane_id: &str,
    mode: &str,
    pattern: &str,
    timeout_ms: Option<u64>,
) -> std::result::Result<PaneWaitResult, PlaneError> {
    let params = wait_for_output_params(pane_id, mode, pattern, timeout_ms)?;
    let seen = pane_recent_text(c, pane_id).unwrap_or_default();
    if seen.contains(pattern) {
        return Ok(PaneWaitResult {
            pane_id: toron_plane::backend::hc::prefix_id(pane_id),
            matched_line: None,
            read: seen,
        });
    }
    let res = c.call("terminal.wait", params).map_err(to_plane_error)?;
    let mut hit = parse_wait_for_output_result(&res)?;
    if hit.read.is_empty() {
        hit.read = pane_recent_text(c, pane_id).unwrap_or_default();
    }
    Ok(hit)
}

/// Spawn shell-readiness gate (bd-pattern-peer-triggered): poll a
/// freshly-split pane until its shell has rendered a prompt, so the launch
/// command lands in an INTERACTIVE shell — not a booting one.
///
/// Why this gate exists: the launch command is typed
/// into the pane's PTY immediately after `terminal.run` returns. A new
/// pane's shell is still starting up (zsh running `.zshrc`, prompt not yet
/// rendered) and the PTY is in CANONICAL mode with a 1024-byte line limit
/// (`MAX_CANON`). The peer-aware template renders to a ~2.6KB single-quoted
/// launch arg; typing it into a booting shell splits it mid-quote, and zsh
/// parks at the PS2 continuation prompt (`>....`) with the agent never
/// starting (2026-08-18 coder2/coder3 dead-on-spawn). The root pane
/// survived only because its first `agent.start` errored (shell not yet an
/// available shell) and the retry's 3s sleep let it finish booting.
///
/// Signal: `terminal.read` recent output contains a rendered prompt — a shell
/// prints its prompt exactly when the line editor is up (raw mode, no
/// canonical line limit). Poll every 250ms until the prompt appears (→
/// `Ok(true)`), the probe errors continuously, or `timeout` elapses (→
/// `Ok(false)` — the caller warns; the old race is the fallback).
pub fn wait_shell_prompt(c: &Client, pane_id: &str, timeout: Duration) -> bool {
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

/// Recent text of a pane (`terminal.read` + `lines`; `ReadResult.text`).
///
/// `pub(crate)`: the pane-wedge probe (bd-pane-wedge-detect-respawn-pnyi)
/// reads the same tail the shell-readiness gate does, so the two can never
/// disagree about what a pane is showing.
pub(crate) fn pane_recent_text(c: &Client, pane_id: &str) -> Result<String> {
    let res = c.terminal_read(pane_id, 30).map_err(|e| anyhow!("{e}"))?;
    Ok(res
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
            if t.starts_with(['❯', '➜', '%', '$', '#', '>', '»', '›', 'λ'])
                || t.ends_with(['❯', '➜', '%', '$', '#', '>', '»', '›', 'λ'])
            {
                return true;
            }
            t.contains('@') && (t.contains(':') || t.contains('~') || t.contains('/'))
        }
        None => false,
    }
}

/// `worktree.list` — hc 0.1.18 (tested) exposes no worktree method, and a terminal
/// backend has no linked-worktree concept, so the honest answer is empty:
/// the preflight's `linked_worktrees` filter stays green and `flywheel
/// worktree list` prints "no worktrees". (The `worktree_create` /
/// `worktree_remove` client methods already return Unsupported.)
pub fn worktree_list(_c: &Client, _workspace_id: Option<&str>) -> Result<Vec<WorktreeInfo>> {
    Ok(Vec::new())
}

/// One foreground process in a pane (from `terminal.get` `foreground`).
///
/// The agent process (opencode/pi/grok/omp) appears here with `pid ==
/// foreground_process_group_id` while it runs; when the process dies the pane
/// falls back to the shell prompt and the agent binary disappears from the
/// list. This is the ground-truth liveness signal `flywheel recover` probes
/// (a pane can exist in hc while the process inside it is dead).
// serde wire shape: the liveness read keeps pid for diagnostics.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct PaneProcessInfoProcess {
    pub pid: u64,
    pub name: String,
    #[serde(default)]
    pub argv0: Option<String>,
}

/// Result of the `terminal.get` liveness read (bd-1np). `foreground_processes` lists the
/// the group leader (`pid == foreground_process_group_id`); the `shell_pid`
/// is the pane's shell, which reclaims the foreground after the agent exits.
// serde wire shape: parsed from `terminal.get`; only the process list is read.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct PaneProcessInfo {
    pub pane_id: String,
    #[serde(default)]
    pub foreground_process_group_id: Option<u64>,
    #[serde(default)]
    pub shell_pid: Option<u64>,
    #[serde(default)]
    pub foreground_processes: Vec<PaneProcessInfoProcess>,
}

/// Process liveness probe (bd-1np) via `terminal.get`: `Terminal.pid` is
/// the shell, `foreground.{pid,name,argv}` is the foreground group leader
/// (the agent while it lives; the shell reclaims it after the agent exits).
/// Parsed from the raw result — toron-core's `Terminal` drops the
/// `foreground` object, and it is the only liveness signal here.
pub fn pane_process_info(c: &Client, pane_id: &str) -> Result<PaneProcessInfo> {
    let res = c.call(
        "terminal.get",
        json!({ "terminal": toron_plane::backend::hc::strip_id_prefix(pane_id) }),
    )?;
    let shell_pid = res.get("pid").and_then(Value::as_u64);
    let (foreground_process_group_id, foreground_processes) = match res.get("foreground") {
        Some(fg) if !fg.is_null() => {
            let pid = fg.get("pid").and_then(Value::as_u64);
            let procs = match (pid, fg.get("name").and_then(Value::as_str)) {
                (Some(pid), Some(name)) => vec![PaneProcessInfoProcess {
                    pid,
                    name: name.to_string(),
                    argv0: fg
                        .get("argv")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }],
                _ => Vec::new(),
            };
            (pid, procs)
        }
        _ => (None, Vec::new()),
    };
    Ok(PaneProcessInfo {
        pane_id: toron_plane::backend::hc::prefix_id(toron_plane::backend::hc::strip_id_prefix(
            pane_id,
        )),
        foreground_process_group_id,
        shell_pid,
        foreground_processes,
    })
}

/// Known agent-kind → process-name mapping for the liveness probe (bd-1np).
/// These are the canonical executables the harness launch runs per kind;
/// the foreground process's `name` / `argv0` matches one of these while
/// the agent lives.
pub fn kind_process_names(kind: &str) -> &'static [&'static str] {
    match kind {
        "opencode" => &["opencode"],
        "pi" => &["pi", "gemini"],
        "grok" => &["grok", "grok-build"],
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

/// Pure liveness verdict from a `terminal.get` snapshot (bd-1np).
///
/// The agent process is alive iff its binary appears in the pane's foreground
/// processes (hc's process-info lists the whole foreground group; the live
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

/// Spawn-to-dispatch readiness gate (bd-eign.6): poll `terminal.get`
///
/// hc's `agent.start` returns once the launch command is dispatched, NOT
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
    c: &Client,
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

/// Responsiveness verdict for a live-but-possibly-frozen agent (bd-a8i).
///
/// `pane_agent_alive` proves the process exists and owns the foreground
/// group; a frozen agent (stuck TUI, no event-loop progress) still passes
/// that check. Responsiveness uses hc's per-pane `state_change_seq`: a
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
pub fn probe_agent_responsiveness(c: &Client, pane_id: &str, window: Duration) -> Responsiveness {
    let sample = |c: &Client| -> Option<(u64, bool)> {
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

#[allow(clippy::large_enum_variant)]
enum StreamItem {
    Event(PushEvent),
    Closed,
    /// hc subscriber fell behind — typed `events_lost` (toron surfaces it).
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

/// Subscribe to hc push events, returning the flywheel-shaped stream.
/// Delegation: the subscription handshake + socket lifecycle are entirely
/// toron-core's. One all-terminals stream (`events.subscribe` takes a single
/// terminal, not a list — one socket, never one per pane); the requested
/// pane ids filter at the mapping edge, so the doorbell never sees
/// unregistered panes (which fell back to pin=pane_id and injected phantom
/// pins). An empty subscription list means all panes (current behavior).
pub fn subscribe(c: &Client, subscriptions: &[Subscription]) -> Result<EventStream> {
    let (_snapshot, mut inner) = c
        .subscribe_terminals(None)
        .map_err(|e| anyhow!("hc subscribe_terminals failed: {e}"))?;
    let allowed: std::collections::HashSet<String> = subscriptions
        .iter()
        .map(|s| toron_plane::backend::hc::strip_id_prefix(&s.pane_id).to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let (tx, rx) = std::sync::mpsc::channel::<StreamItem>();
    std::thread::spawn(move || {
        pump_backend_events(|| inner.read_event(), &tx, &allowed);
    });
    Ok(EventStream { rx, timeout: None })
}

fn push_from_backend(
    ev: toron_plane::backend::types::BackendEvent,
    allowed: &std::collections::HashSet<String>,
) -> Option<PushEvent> {
    use toron_plane::backend::types::BackendEvent;
    let pane_in_scope = |pane: &toron_plane::backend::types::PaneId| {
        allowed.is_empty()
            || allowed.contains(toron_plane::backend::hc::strip_id_prefix(pane.as_str()))
    };
    match ev {
        BackendEvent::AgentStatusChanged {
            pane,
            agent,
            status,
        } => {
            if !pane_in_scope(&pane) {
                return None;
            }
            let change = PaneAgentStatusChanged {
                pane_id: pane.as_str().to_string(),
                workspace_id: String::new(),
                agent_status: status,
                agent,
                display_agent: None,
                state_labels: None,
                title: None,
            };
            Some(PushEvent {
                kind: TerminalEventKind::PaneAgentStatusChangedSub,
                data: TerminalEventData::PaneAgentStatusChangedSub(change),
            })
        }
        BackendEvent::PaneExited { pane } => {
            if !pane_in_scope(&pane) {
                return None;
            }
            Some(PushEvent {
                kind: TerminalEventKind::PaneExited,
                data: TerminalEventData::PaneExited(PaneExited {
                    pane_id: pane.as_str().to_string(),
                    workspace_id: String::new(),
                }),
            })
        }
        BackendEvent::AgentDetected { .. } | BackendEvent::EventsLost | BackendEvent::Raw(_) => {
            None
        }
    }
}

fn pump_backend_events<R>(
    mut read: R,
    tx: &std::sync::mpsc::Sender<StreamItem>,
    allowed: &std::collections::HashSet<String>,
) where
    R: FnMut() -> Result<Option<toron_plane::backend::types::BackendEvent>, BackendError>,
{
    use toron_plane::backend::types::BackendEvent;
    loop {
        match read() {
            Ok(Some(BackendEvent::EventsLost)) => {
                if tx.send(StreamItem::EventsLost).is_err() {
                    break;
                }
            }
            Ok(Some(ev)) => {
                if let Some(push) = push_from_backend(ev, allowed)
                    && tx.send(StreamItem::Event(push)).is_err()
                {
                    break;
                }
            }
            Ok(None) => {
                let _ = tx.send(StreamItem::Closed);
                break;
            }
            Err(e) if e.code == BackendErrorCode::EventsLost => {
                if tx.send(StreamItem::EventsLost).is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = tx.send(StreamItem::Error(format!("hc read failed: {e}")));
                break;
            }
        }
    }
}

/// Gap history (bd-v9k6), kept with the live loop: a gap used to BREAK the
/// reader, which killed the subscription — the doorbell cleared coverage
/// and re-subscribed (a NEW socket per gap; the fresh subscriber started
/// BEHIND the ring, so it was told `events_lost` again). The loop below
/// delivers the gap in band and keeps reading instead.
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
                    anyhow!("hc stream read timed out")
                }
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    anyhow!("hc stream closed")
                }
            }),
            None => self.rx.recv().map_err(|_| anyhow!("hc stream closed")),
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
    use toron_plane::backend::types::{BackendEvent, PaneId};

    /// bd-v9k6 on the LIVE loop: a gap is a NOTICE, not a death — delivered
    /// in band, and the loop keeps reading past it (same contract the dead
    /// `pump_events` loop pinned; the coverage migrates with the code).
    #[test]
    fn a_gap_is_delivered_in_band_and_the_stream_keeps_reading() {
        let (tx, rx) = std::sync::mpsc::channel::<StreamItem>();
        let allowed = std::collections::HashSet::new();
        let mut calls = 0u32;
        pump_backend_events(
            || {
                calls += 1;
                match calls {
                    1 => Err(BackendError::new(
                        BackendErrorCode::EventsLost,
                        "subscriber fell behind",
                    )),
                    2 => Ok(Some(BackendEvent::AgentStatusChanged {
                        pane: PaneId::new("hc:t7"),
                        agent: Some("opencode".to_string()),
                        status: AgentStatus::Idle,
                    })),
                    _ => Ok(None),
                }
            },
            &tx,
            &allowed,
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

    /// Detected pushes carry no routing fact: they drop at the edge so the
    /// push surface stays the two routed kinds (consumers ignored them via
    /// wildcard arms, so the drop changes no behavior).
    #[test]
    fn agent_detected_drops_at_the_edge() {
        let allowed = std::collections::HashSet::new();
        let ev = BackendEvent::AgentDetected {
            pane: PaneId::new("hc:t7"),
            agent: Some("opencode".to_string()),
            status: AgentStatus::Working,
        };
        assert!(push_from_backend(ev, &allowed).is_none());
    }

    /// Steering 6: two panes in the stream, one subscribed — only the
    /// subscribed pane reaches the channel (single socket, filter at the
    /// mapping edge). Empty set = all panes (existing behavior).
    #[test]
    fn subscribe_filter_drops_unsubscribed_panes() {
        use std::collections::HashSet;
        fn status_for(pane: &str) -> BackendEvent {
            BackendEvent::AgentStatusChanged {
                pane: PaneId::new(pane),
                agent: Some("opencode".to_string()),
                status: AgentStatus::Idle,
            }
        }
        let (tx, rx) = std::sync::mpsc::channel::<StreamItem>();
        let allowed: HashSet<String> = ["t8".to_string()].into_iter().collect();
        let mut calls = 0u32;
        pump_backend_events(
            || {
                calls += 1;
                match calls {
                    1 => Ok(Some(status_for("hc:t7"))),
                    2 => Ok(Some(status_for("hc:t8"))),
                    _ => Ok(None),
                }
            },
            &tx,
            &allowed,
        );
        let panes: Vec<String> = rx
            .try_iter()
            .filter_map(|i| match i {
                StreamItem::Event(e) => match e.data {
                    TerminalEventData::PaneAgentStatusChangedSub(c) => Some(c.pane_id),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(panes, vec!["hc:t8".to_string()]);
        let (tx, rx) = std::sync::mpsc::channel::<StreamItem>();
        let allowed = HashSet::new();
        let mut calls = 0u32;
        pump_backend_events(
            || {
                calls += 1;
                match calls {
                    1 => Ok(Some(status_for("hc:t7"))),
                    2 => Ok(Some(status_for("hc:t8"))),
                    _ => Ok(None),
                }
            },
            &tx,
            &allowed,
        );
        let panes: Vec<String> = rx
            .try_iter()
            .filter_map(|i| match i {
                StreamItem::Event(e) => match e.data {
                    TerminalEventData::PaneAgentStatusChangedSub(c) => Some(c.pane_id),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(panes, vec!["hc:t7".to_string(), "hc:t8".to_string()]);
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
            (
                "dead_process_shell_foreground_classified_dead",
                "opencode",
                info(Some(50), Some(50), vec![proc("zsh", 50)]),
                Some(false),
            ),
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
            (
                "empty_foreground_classified_dead",
                "opencode",
                info(Some(50), Some(50), vec![]),
                Some(false),
            ),
            (
                "unknown_kind_inconclusive",
                "mystery-kind",
                info(Some(200), Some(50), vec![proc("mystery-agent", 200)]),
                None,
            ),
            (
                "unknown_kind_shell_foreground_dead",
                "mystery-kind",
                info(Some(50), Some(50), vec![proc("zsh", 50)]),
                Some(false),
            ),
            (
                "kind_alias_matches",
                "grok",
                info(Some(300), Some(50), vec![proc("grok-build", 300)]),
                Some(true),
            ),
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
mod plane_launch_env_tests {
    use super::{apply_opencode_go_api_key_alias, plane_launch_env};

    fn cfg() -> crate::config::Config {
        crate::config::Config::default()
    }

    /// bd-yd74j: the pane runs in the SPAWNER's state roots. The rex server
    /// that splits the pane has the operator's env, so without these the pane
    /// (and its doorbell host) writes live state under a sandboxed run.
    #[test]
    fn env_carries_the_spawners_state_roots() {
        let _g = crate::config::lock_env();
        unsafe {
            std::env::set_var("HOME", "/tmp/sandbox-home");
            std::env::set_var("XDG_STATE_HOME", "/tmp/sandbox-state");
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::set_var("TORON_DATA_DIR", "/tmp/sandbox-toron");
        }
        let env = plane_launch_env(&cfg(), None);
        assert_eq!(
            env.get("HOME").map(String::as_str),
            Some("/tmp/sandbox-home")
        );
        assert_eq!(
            env.get("XDG_STATE_HOME").map(String::as_str),
            Some("/tmp/sandbox-state")
        );
        assert_eq!(
            env.get("TORON_DATA_DIR").map(String::as_str),
            Some("/tmp/sandbox-toron")
        );
        assert!(
            !env.contains_key("XDG_CONFIG_HOME"),
            "an absent var is not invented"
        );
        unsafe {
            std::env::remove_var("HOME");
            std::env::remove_var("XDG_STATE_HOME");
            std::env::remove_var("TORON_DATA_DIR");
        }
    }

    /// bd-pin-public-plane-s-easy.2: the pane env carries the agent's mail
    /// PIN as AGENT_NAME + FLYWHEEL_MAIL_AS — the identity the spawned agent
    /// boots knowing it is addressed by (same pin the peer template injects
    /// and the loop substitutes for {AGENT_NAME}).
    #[test]
    fn env_carries_pin_as_agent_name_and_mail_as() {
        let env = plane_launch_env(&cfg(), Some("DustySparrow"));
        assert!(
            env.get("HC_SOCKET_PATH")
                .map(|s| s.contains("herdr-core"))
                .unwrap_or(false)
        );
        assert_eq!(env.get("HC_SESSION").map(String::as_str), Some("default"));
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
        let env = plane_launch_env(&cfg(), None);
        assert!(env.contains_key("HC_SOCKET_PATH"));
        assert!(!env.contains_key("AGENT_NAME"));
        assert!(!env.contains_key("FLYWHEEL_MAIL_AS"));
    }

    #[test]
    fn env_ignores_empty_pin() {
        let env = plane_launch_env(&cfg(), Some(""));
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
        let env = plane_launch_env(&cfg(), None);
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
            (
                "frozen_seq_idle_unresponsive",
                10,
                10,
                3,
                true,
                Responsiveness::Unresponsive,
            ),
            (
                "frozen_seq_busy_inconclusive",
                10,
                10,
                3,
                false,
                Responsiveness::Inconclusive,
            ),
            (
                "zero_window_inconclusive",
                10,
                10,
                0,
                true,
                Responsiveness::Inconclusive,
            ),
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
            "sqlite3 query failed on {}: {}",
            db.display(),
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
                .unwrap_or_else(|| cap(&v.to_string(), 2000))
        })
        .unwrap_or_default();
    let output = state
        .get("output")
        .map(|v| {
            v.as_str()
                .map(|s| cap(s, 4000))
                .unwrap_or_else(|| cap(&v.to_string(), 4000))
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
    summarize_part_value(&v)
}

/// The same summarizer over an ALREADY-parsed part. opencode v2 nests parts
/// in `session_message.data.content[]`, so the reader has values, not strings.
fn summarize_part_value(v: &Value) -> Option<Value> {
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
            "tool": v
                .get("name")
                .or_else(|| v.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or("?"),
            "state": summarize_tool_state(v.get("state").unwrap_or(&Value::Null)),
        })),
        _ => None,
    }
}

/// bd-5t31z: which session-store layout a store exposes, decided from its
/// table list BEFORE any query runs. Pure, so the drift is testable without
/// a database and without depending on the installed sqlite3.
#[derive(Debug, PartialEq, Eq)]
enum SessionSchema {
    /// The v1 layout this reader speaks first: `session` + `part`.
    V1,
    /// opencode v2 (2026-10): `session_v2` + `session_message`. The message
    /// row's `data.content[]` carries the parts; `session_part` exists but is
    /// empty in the measured store (8444 messages, 0 parts).
    V2,
    /// Anything else — name the tables rather than blame sqlite.
    Drift { present: Vec<String> },
}

fn session_schema_from_tables(tables: &[String]) -> SessionSchema {
    let has = |t: &str| tables.iter().any(|x| x == t);
    if has("session") && has("part") {
        SessionSchema::V1
    } else if has("session_v2") && has("session_message") {
        SessionSchema::V2
    } else {
        SessionSchema::Drift {
            present: tables.to_vec(),
        }
    }
}

/// The typed failure a drifted store earns: the DB, the plane, the layouts
/// this reader speaks, and the tables the store DOES carry. Pure so the
/// wording is pinned by a test.
fn session_store_drift_error(db: &Path, present: &[String]) -> String {
    format!(
        "opencode session store drifted at {} (plane: opencode structured session store): \
         no `session`+`part` (v1) and no `session_v2`+`session_message` (v2) tables, so nothing \
         was queried. The store carries [{}] (bd-5t31z — map the newer layout before trusting \
         this output)",
        db.display(),
        present.join(", ")
    )
}

/// The store's table list, read through the same sqlite3 shell the queries
/// use. An error here is a real read failure (the file is opened), not drift.
fn session_tables(db: &Path) -> Result<Vec<String>> {
    let rows = sqlite_json(
        db,
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
    )?;
    Ok(rows
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
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
    let schema = session_schema_from_tables(&session_tables(&db)?);
    if let SessionSchema::Drift { present } = &schema {
        bail!("{}", session_store_drift_error(&db, present));
    }
    let session_table = match schema {
        SessionSchema::V1 => "session",
        SessionSchema::V2 => "session_v2",
        SessionSchema::Drift { .. } => unreachable!("drift returns above"),
    };
    let (sid, meta): (String, Value) = if let Some(sid) = session_id {
        let rows = sqlite_json(
            &db,
            &format!(
                "SELECT id, title, agent, model, time_updated FROM {session_table} WHERE id='{}'",
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
            "SELECT id, title, agent, model, time_updated FROM {session_table} \
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
    let parts: Vec<Value> = match schema {
        SessionSchema::V1 => {
            let parts_sql = format!(
                "SELECT data FROM part WHERE session_id='{}' ORDER BY time_created, id LIMIT {}",
                sq(&sid),
                limit.max(1)
            );
            sqlite_json(&db, &parts_sql)?
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|row| row["data"].as_str().and_then(summarize_part))
                .collect()
        }
        SessionSchema::V2 => {
            let msg_sql = format!(
                "SELECT data FROM session_message WHERE session_id='{}' ORDER BY seq LIMIT {}",
                sq(&sid),
                limit.max(1)
            );
            sqlite_json(&db, &msg_sql)?
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|row| row["data"].as_str())
                .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
                .flat_map(|msg| {
                    msg.get("content")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|part| summarize_part_value(&part))
                .collect()
        }
        SessionSchema::Drift { .. } => unreachable!("drift returns above"),
    };
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

#[cfg(test)]
mod session_schema_tests {
    use super::*;

    /// bd-5t31z: `flywheel output` assumed a `session` table and died with
    /// sqlite's argument parse error, naming neither the store nor the plane.
    /// This drives the REAL reader against a drifted fixture store, so
    /// deleting the preflight (which re-enters the `session` query) turns the
    /// message assertions red.
    #[test]
    fn output_reports_a_missing_session_table() {
        let drifted = vec!["session_part".to_string(), "session_v2".to_string()];
        assert_eq!(
            session_schema_from_tables(&drifted),
            SessionSchema::Drift {
                present: drifted.clone()
            }
        );
        let v1 = vec!["part".to_string(), "session".to_string()];
        assert_eq!(session_schema_from_tables(&v1), SessionSchema::V1);
        let v2_schema = vec![
            "session_v2".to_string(),
            "session_message".to_string(),
            "session_part".to_string(),
        ];
        assert_eq!(session_schema_from_tables(&v2_schema), SessionSchema::V2);
        assert!(matches!(
            session_schema_from_tables(&["session".to_string()]),
            SessionSchema::Drift { .. }
        ));

        let dir = std::env::temp_dir().join(format!("fw-output-drift-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let db = dir.join("opencode.db");
        let _ = std::fs::remove_file(&db);
        let created = std::process::Command::new("sqlite3")
            .arg(&db)
            .arg("CREATE TABLE session_v2 (id text PRIMARY KEY); CREATE TABLE session_part (data text);")
            .status()
            .expect("sqlite3 must be installed — the reader shells it too");
        assert!(created.success(), "fixture store created");

        // SAFETY: OPENCODE_DB has exactly one reader (this module) and no
        unsafe { std::env::set_var("OPENCODE_DB", &db) };
        let err = read_session_log("/nonexistent-dir", None, None, 10)
            .expect_err("a drifted store must fail, never read as an empty session");
        unsafe { std::env::remove_var("OPENCODE_DB") };
        let _ = std::fs::remove_file(&db);

        let msg = err.to_string();
        assert!(msg.contains("opencode session store drifted"), "{msg}");
        assert!(
            msg.contains(&db.display().to_string()),
            "names the store: {msg}"
        );
        assert!(
            msg.contains("plane: opencode structured session store"),
            "names the plane: {msg}"
        );
        assert!(
            msg.contains("session_v2"),
            "names what the store does carry: {msg}"
        );
        assert!(
            !msg.contains("Parse error"),
            "sqlite's argument noise must not be the diagnosis: {msg}"
        );

        let db2 = dir.join("opencode-v2.db");
        let _ = std::fs::remove_file(&db2);
        let msg1 = json!({
            "type": "assistant",
            "content": [
                {"type": "text", "text": "hello v2"},
                {"type": "tool", "name": "shell",
                 "state": {"status": "completed", "input": {"command": "ls"}, "output": "ok"}}
            ]
        })
        .to_string();
        let msg2 = json!({
            "type": "assistant",
            "content": [
                {"type": "reasoning", "text": "thinking"},
                {"type": "step-start"}
            ]
        })
        .to_string();
        let q = |s: &str| s.replace('\'', "''");
        let sql = format!(
            "CREATE TABLE session_v2 (id text PRIMARY KEY, title text, agent text, \
             model text, directory text, time_updated integer); \
             CREATE TABLE session_message (id text PRIMARY KEY, session_id text, \
             seq integer, data text); \
             INSERT INTO session_v2 VALUES ('ses_test','V2 fixture','build','model-x', \
             '/tmp/v2proj',123); \
             INSERT INTO session_message VALUES ('m1','ses_test',1,'{}'); \
             INSERT INTO session_message VALUES ('m2','ses_test',2,'{}');",
            q(&msg1),
            q(&msg2)
        );
        let created = std::process::Command::new("sqlite3")
            .arg(&db2)
            .arg(&sql)
            .status()
            .expect("sqlite3 must be installed — the reader shells it too");
        assert!(created.success(), "v2 fixture store created");
        // SAFETY: same single-reader invariant as above; this function is the
        unsafe { std::env::set_var("OPENCODE_DB", &db2) };
        let got = read_session_log("/tmp/v2proj", None, None, 10).expect("v2 store reads");
        unsafe { std::env::remove_var("OPENCODE_DB") };
        let _ = std::fs::remove_file(&db2);
        assert_eq!(got["session_id"], "ses_test");
        assert_eq!(got["title"], "V2 fixture");
        let parts = got["parts"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            parts.len(),
            3,
            "text + tool + reasoning, step-start skipped: {parts:?}"
        );
        assert_eq!(parts[0]["kind"], "text");
        assert_eq!(parts[0]["text"], "hello v2");
        assert_eq!(parts[1]["kind"], "tool");
        assert_eq!(parts[1]["tool"], "shell", "v2 names the tool `name`");
        assert_eq!(parts[2]["kind"], "reasoning");
    }
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

/// bd-swarm-cockpit-pczs: the last `n` actions from the session hc
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
            parts.reverse();
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
    /// file, then read the file back (hc docs' file-path contract).
    fn file_fallback(&mut self) -> Result<String, BackendError>;
}

/// Bounded wait budget for `wait_idle` (hc `agent.wait` timeout).
pub const ALT_SCREEN_IDLE_WAIT_MS: u64 = 5_000;

/// hc wire `agent.read` source for unwrapped transcript history (the
/// docs call it `recent-unwrapped`; the protocol enum is
/// `recent_unwrapped` — verified live against hc
/// 0.8.0-preview.2026-08-04-d78e3d3b5126, 2026-08-15).
pub const RECENT_UNWRAPPED_SOURCE: &str = "recent_unwrapped";

/// H4 (bd-harness-alt-screen-reads): the alt-screen transcript read ladder.
///
/// 1. `recent-unwrapped` (the transcript source — wire name
///    `recent_unwrapped`; hc auto-scrolls the mouse-scroll interface when
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
    if let Ok(t) = ops.read("visible") {
        return Ok(t);
    }
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

/// H4 (bd-harness-alt-screen-reads): adapter over the toron-core hc
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
    use toron_plane::backend::error::{BackendError, BackendErrorCode};

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
            source: "hc:pi".into(),
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

/// H3 (bd-harness-integration-preflight): install-target mapping for the
/// harness kinds. hc 0.1.18 (tested) exposes no integration surface, so the status
/// parsers are deleted (nothing to parse) and every live status is
/// `not_installed` — the gate is `cargo test -p flywheel integration_status`.
#[cfg(test)]
mod integration_status_tests {
    use super::{INTEGRATION_KINDS, IntegrationStatus, integration_target};

    /// Every canonical kind maps to a target; the underscore twin
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
        assert!(!shell_prompt_rendered(
            "Last login: Mon Aug 18 04:24:39 on ttys001\n"
        ));
        assert!(!shell_prompt_rendered(
            "loading zsh config...\ngit: command not found"
        ));
    }

    #[test]
    fn rendered_prompt_sigils_are_ready() {
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
    /// bare project label, the flywheel-OWNED label must win even when hc
    /// reports NO agent names (the omp case — the name scan finds nothing
    /// and the old code fell back to list order, dispatching into a foreign
    /// workspace: "no agents in workspace w37").
    #[test]
    fn resolve_workspace_id_prefers_owned_label_over_foreign() {
        use crate::hc::{AgentInfo, AgentStatus, WorkspaceInfo};
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
        let foreign = mk("w37", "toron");
        let owned = mk("w70", "flywheel-toron");
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
        assert_eq!(
            super::resolve_workspace_id(
                &[foreign.clone(), owned.clone()],
                &no_agents,
                "flywheel-toron-"
            ),
            "w70"
        );
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
        use crate::hc::{AgentInfo, AgentStatus, WorkspaceInfo};
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

        assert_eq!(
            super::resolve_workspace_id(&[stale.clone(), live.clone()], &agents, "flywheel-toron-"),
            "w23",
            "the prefix scan cannot see a pin-named crew, so list order wins"
        );

        assert_eq!(
            super::resolve_workspace_from_registry(&[stale.clone(), live.clone()], &agents, &[]),
            None
        );
    }
}

#[cfg(test)]
mod gs049_launch_pending_tests {
    use super::*;
    use toron_plane::backend::types::AgentStatus;

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
    #[ignore = "bd-d7zu LIVE probe: needs a live hc daemon"]
    fn real_pane_reaches_a_detected_prompt() {
        let cfg = crate::Config::load().expect("config");
        let c = Client::connect(Some(&cfg.socket_path())).expect("hc connect");
        let (ws, root) = c
            .workspace_create(Some("/tmp"), Some("flywheel-shell-gate-e2e"), false, None)
            .expect("workspace_create");
        let ws_id = ws.workspace_id.clone();
        let root = root.expect("workspace_create returns a root pane").pane_id;
        let split = pane_split(&c, &root, "down", Some("/tmp"), None).expect("pane_split");
        let pane = split.pane_id.clone();

        let started = std::time::Instant::now();
        let ready = wait_shell_prompt(&c, &pane, Duration::from_secs(15));
        let elapsed = started.elapsed();
        let matched_on = pane_recent_text(&c, &pane)
            .ok()
            .and_then(|t| {
                t.lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string())
            })
            .unwrap_or_default();

        let _ = c.workspace_close(&ws_id, true);
        let _ = c.terminal_kill(&pane);

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

/// `__wait-output` against a REAL pane: hc's `output` wait only sees output
/// printed after the wait starts, so the read-first check is what makes a
/// pattern already on screen match. Ignored by default (needs a live daemon).
#[cfg(test)]
mod wait_output_live {
    use super::*;

    #[test]
    #[ignore = "LIVE probe: needs a live hc daemon"]
    fn a_pattern_already_on_screen_matches_without_waiting() {
        let cfg = crate::Config::load().expect("config");
        let c = Client::connect(Some(&cfg.socket_path())).expect("hc connect");
        let (ws, root) = c
            .workspace_create(Some("/tmp"), Some("flywheel-wait-output-live"), false, None)
            .expect("workspace_create");
        let ws_id = ws.workspace_id.clone();
        let root = root.expect("workspace_create returns a root pane").pane_id;
        let split = pane_split(&c, &root, "down", Some("/tmp"), None).expect("pane_split");
        let pane = split.pane_id.clone();
        assert!(
            wait_shell_prompt(&c, &pane, Duration::from_secs(15)),
            "shell never reached a prompt"
        );
        c.pane_run(&pane, "printf 'WAIT-OUTPUT-NEEDLE\\n'; sleep 30")
            .expect("run");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut visible = false;
        while std::time::Instant::now() < deadline {
            visible = pane_recent_text(&c, &pane)
                .unwrap_or_default()
                .contains("WAIT-OUTPUT-NEEDLE");
            if visible {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(visible, "needle never reached the screen");

        let started = std::time::Instant::now();
        let hit = pane_wait_output(&c, &pane, "match", "WAIT-OUTPUT-NEEDLE", Some(500))
            .expect("already-visible needle must match");
        let elapsed = started.elapsed();
        let _ = c.workspace_close(&ws_id, true);
        assert!(
            c.terminal_get(&pane).is_err(),
            "child {pane} survived workspace close"
        );
        assert!(
            c.terminal_get(&root).is_err(),
            "root {root} survived workspace close"
        );
        assert!(hit.read.contains("WAIT-OUTPUT-NEEDLE"), "{hit:?}");
        assert!(
            elapsed < Duration::from_millis(500),
            "matched only after the wait timeout — the read-first check did not run ({elapsed:?})"
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

/// Compatibility surface for call sites that still speak the old hc Client
/// method names. Prefer MultiplexerBackend (rex primary / hc fallback) for new
/// code. Methods that hc does not implement call through `call`/`call_coded`
/// and surface `missing_method` from the server, or return Unsupported-shaped
/// BackendError for workspace/tab ops that belong on rex.
fn unsupported_on_hc(method: &str) -> BackendError {
    BackendError::new(
        BackendErrorCode::Unsupported,
        format!("hc Client has no {method}; use rex MultiplexerBackend"),
    )
}

/// Label stamped on every workspace shell so `workspace_list` can find it
/// again (hc has no workspaces — the shell terminal IS the workspace).
const WORKSPACE_LABEL_KEY: &str = "flywheel_workspace";

fn workspace_info_for(term: &toron_plane::backend::hc::Terminal) -> Option<WorkspaceInfo> {
    let label = term.labels.get(WORKSPACE_LABEL_KEY)?.clone();
    Some(WorkspaceInfo {
        workspace_id: toron_plane::backend::hc::prefix_id(&term.address),
        number: 0,
        label,
        focused: false,
        pane_count: 0,
        tab_count: 0,
        active_tab_id: String::new(),
        agent_status: AgentStatus::Unknown,
        cwd: term.cwd.clone(),
    })
}

impl Client {
    /// hc has no workspaces: the workspace shells (stamped
    /// [`WORKSPACE_LABEL_KEY`] at create) listed as workspaces. Empty when
    /// none exists — so list-then-create flows create.
    pub fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, BackendError> {
        let terms = self.terminal_list(false)?;
        Ok(terms.iter().filter_map(workspace_info_for).collect())
    }
    /// A workspace is a shell terminal: `terminal.run` with cwd + env and NO
    /// argv (the server runs its own shell). The id IS the address; the
    /// label rides [`WORKSPACE_LABEL_KEY`] so `workspace_list` (and hence
    /// `find_workspace`) sees it. Returns the root pane too.
    pub fn workspace_create(
        &self,
        cwd: Option<&str>,
        label: Option<&str>,
        _focus: bool,
        env: Option<&std::collections::BTreeMap<String, String>>,
    ) -> Result<(WorkspaceInfo, Option<PaneInfo>), BackendError> {
        let mut params = serde_json::Map::new();
        if let Some(cwd) = cwd {
            params.insert("cwd".into(), json!(cwd));
        }
        let mut owned = env.cloned().unwrap_or_default();
        owned
            .entry("FLYWHEEL_ROLE".to_string())
            .or_insert_with(|| "crew".to_string());
        params.insert("env".into(), json!(owned));
        if let Some(label) = label {
            params.insert("labels".into(), json!({ WORKSPACE_LABEL_KEY: label }));
        }
        params.insert("no_parent".into(), serde_json::json!(true));
        let res: toron_plane::backend::hc::Terminal = serde_json::from_value(
            self.call("terminal.run", Value::Object(params))
                .map_err(|e| {
                    BackendError::new(
                        e.code().clone(),
                        format!("terminal.run (workspace shell): {}", e.message),
                    )
                })?,
        )
        .map_err(|e| {
            BackendError::new(
                toron_plane::backend::error::BackendErrorCode::Malformed,
                format!("parse terminal.run result: {e}"),
            )
        })?;
        let ws = workspace_info_for(&res).unwrap_or(WorkspaceInfo {
            workspace_id: toron_plane::backend::hc::prefix_id(&res.address),
            number: 0,
            label: label.unwrap_or_default().to_string(),
            focused: false,
            pane_count: 0,
            tab_count: 0,
            active_tab_id: String::new(),
            agent_status: AgentStatus::Unknown,
            cwd: res.cwd.clone(),
        });
        let want = ws.workspace_id.clone();
        let root = self
            .pane_list()
            .map(|panes| panes.into_iter().find(|p| p.pane_id == want))?;
        Ok((ws, root))
    }
    /// A workspace is a root shell terminal plus its descendants. Children
    /// are created `outlive_parent`, so the root's death does not end them —
    /// the shared client kills and removes every pane in the tree.
    pub fn workspace_close(
        &self,
        workspace_id: &str,
        _close_group: bool,
    ) -> Result<(), BackendError> {
        self.inner.close_workspace(workspace_id)
    }
    /// Run `command` IN the pane: `terminal.send` pastes the text and presses
    /// Enter in that terminal (hc has no remote-run — spawning a sibling
    /// terminal here orphaned a shell per call).
    pub fn pane_run(&self, pane_id: &str, command: &str) -> Result<(), BackendError> {
        self.terminal_send(pane_id, command, None).map(|_| ())
    }
    pub fn pane_send_text(&self, pane_id: &str, text: &str) -> Result<(), BackendError> {
        self.terminal_send(pane_id, text, None).map(|_| ())
    }
    pub fn pane_send_input(&self, pane_id: &str, keys: &[&str]) -> Result<(), BackendError> {
        self.terminal_keys(pane_id, keys).map(|_| ())
    }
    /// Start the agent IN the existing pane: the shell-quoted `kind + args`
    /// line is typed into the shell via `terminal.send` (which pastes and
    /// presses Enter), so the agent runs as that shell's child with the
    /// pane's env. A second `terminal.run` here orphaned a shell per spawn
    /// and lost the pane env — never do that.
    pub fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        _timeout_ms: u64,
        args: &[String],
    ) -> Result<AgentInfo, BackendError> {
        let _ = name;
        let line = crate::recipe_launch_command(kind, args);
        self.terminal_send(pane_id, &line, None)?;
        self.agent_get(pane_id)
    }
    /// Rename the agent by stamping its mail pin: `terminal.label` with
    /// `toron_pin` = new name (mail routing reads `toron_pin`). hc exposes
    /// no `agent.rename` — never send it.
    pub fn agent_rename(&self, target: &str, new_name: &str) -> Result<(), BackendError> {
        self.call_coded(
            "terminal.label",
            json!({
                "terminal": toron_plane::backend::hc::strip_id_prefix(target),
                "labels": { "toron_pin": new_name },
            }),
        )
        .map(|_| ())
    }
    pub fn agent_prompt(
        &self,
        target: &str,
        text: &str,
        wait: Option<WaitOptions>,
    ) -> Result<AgentInfo, BackendError> {
        self.agent_prompt_coded(target, text, wait)
    }
    pub fn agent_prompt_coded(
        &self,
        target: &str,
        text: &str,
        wait: Option<WaitOptions>,
    ) -> Result<AgentInfo, BackendError> {
        let timeout = wait.as_ref().and_then(|w| w.timeout_ms);
        let _ = self.terminal_send(target, text, timeout)?;
        self.agent_get(target)
    }
    pub fn agent_wait(
        &self,
        target: &str,
        _until: &[AgentStatus],
        timeout_ms: Option<u64>,
    ) -> Result<AgentInfo, BackendError> {
        let _ = self.terminal_wait(target, timeout_ms)?;
        self.agent_get(target)
    }
    pub fn agent_read(
        &self,
        target: &str,
        _source: &str,
        lines: Option<u32>,
    ) -> Result<String, BackendError> {
        let res = self.terminal_read(target, lines.unwrap_or(200))?;
        let text = res
            .get("content")
            .or_else(|| res.get("text"))
            .or_else(|| res.get("output"))
            .and_then(Value::as_str)
            .unwrap_or("");
        Ok(text.to_string())
    }
    pub fn agent_send_keys(&self, target: &str, keys: &[&str]) -> Result<(), BackendError> {
        self.terminal_keys(target, keys).map(|_| ())
    }
    pub fn worktree_create(
        &self,
        _workspace_id: &str,
        _branch: &str,
        _base: Option<&str>,
        _label: Option<&str>,
        _open: bool,
    ) -> Result<Value, BackendError> {
        Err(unsupported_on_hc("worktree_create"))
    }
    pub fn worktree_remove(&self, _workspace_id: &str, _force: bool) -> Result<(), BackendError> {
        Err(unsupported_on_hc("worktree_remove"))
    }
    pub fn ping(&self) -> Result<Value, BackendError> {
        self.terminal_list(false)
            .map(|v| json!({ "ok": true, "terminals": v.len() }))
    }
}

/// Migration witness (hc 0.1.18 (tested) port): every raw RPC literal in
/// `src/hc.rs` + `src/cmd/crew.rs` must name a method the hc schema
/// fixture lists. A pre-hc literal (`pane.split`, `agent.rename`, …)
/// reintroduced here fails this test instead of failing at runtime behind
/// a typed `missing_method`. Cheap + deterministic: two `include_str!`s
/// and the checked-in fixture, no daemon, no network.
#[cfg(test)]
mod hc_method_witness_tests {
    /// Fixture: the authoritative 0.1.18 method list (toron branch).
    const SCHEMA: &str =
        include_str!("../../toron/crates/toron-core/tests/fixtures/hc-schema.json");
    const HC_SUPPORT_SRC: &str = include_str!("hc.rs");
    const CREW_SRC: &str = include_str!("cmd/crew.rs");

    /// Raw RPC literals: the method string of each `.call("…")` /
    /// `.call_coded("…")` site. An occurrence is a call only when the
    /// token after the quote matches `[a-z][a-z0-9_.]*` — anything else
    /// (the scanner's own `.find(".call(\"")` line, prose, a non-literal
    /// call) is skipped so the witness scans itself clean.
    fn rpc_literals(src: &str) -> Vec<String> {
        fn token(after: &str) -> Option<(&str, &str)> {
            let mut len = 0;
            for b in after.bytes() {
                if b.is_ascii_lowercase() || b == b'.' || b == b'_' || len > 0 && b.is_ascii_digit()
                {
                    len += 1;
                } else {
                    break;
                }
            }
            if len > 0 && after.as_bytes()[0].is_ascii_lowercase() {
                Some(after.split_at(len))
            } else {
                None
            }
        }
        let mut out = Vec::new();
        for line in src.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut rest = line;
            loop {
                let next = rest
                    .find(".call(\"")
                    .map(|p| (p, ".call(\"".len()))
                    .or_else(|| {
                        rest.find(".call_coded(\"")
                            .map(|p| (p, ".call_coded(\"".len()))
                    });
                let Some((pos, skip)) = next else { break };
                let after = &rest[pos + skip..];
                match token(after) {
                    Some((method, tail)) => {
                        out.push(method.to_string());
                        rest = tail;
                    }
                    None => rest = after,
                }
            }
        }
        out
    }

    #[test]
    fn scanner_ignores_its_own_source_line() {
        let own = r#"let next = rest.find(".call(\"").map(|p| (p, ".call(\"".len()));"#;
        assert_eq!(rpc_literals(own), Vec::<String>::new());
        assert_eq!(
            rpc_literals(r#"let x = c.call("terminal.kill", v)?;"#),
            vec!["terminal.kill".to_string()]
        );
        assert_eq!(
            rpc_literals(r#"c.call_coded("terminal.label", json!({}));"#),
            vec!["terminal.label".to_string()]
        );
    }

    #[test]
    fn every_raw_rpc_literal_is_an_hc_schema_method() {
        let schema: serde_json::Value =
            serde_json::from_str(SCHEMA).expect("hc schema fixture parses");
        let methods: std::collections::HashSet<String> = schema
            .get("methods")
            .and_then(|m| m.as_array())
            .expect("schema has methods[]")
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
            .map(str::to_string)
            .collect();
        assert!(
            methods.contains("terminal.run"),
            "fixture must list terminal.run (sanity — wrong fixture?)"
        );
        for (name, src, must_find) in [
            ("hc.rs", HC_SUPPORT_SRC, true),
            ("cmd/crew.rs", CREW_SRC, false),
        ] {
            let literals = rpc_literals(src);
            assert!(
                !must_find || !literals.is_empty(),
                "{name}: scanner found no RPC literals (scanner broken?)"
            );
            for method in literals {
                assert!(
                    methods.contains(&method),
                    "{name}: raw RPC {method:?} is not an hc 0.1.18 (tested) schema method — port it or delete it"
                );
            }
        }
    }
}
