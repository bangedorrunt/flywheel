//! herdr backend client — newline-delimited JSON-RPC over a unix socket.
//!
//! Protocol: each request is one JSON object per line:
//!   {"id": "...", "method": "agent.prompt", "params": {...}}
//! Responses match the request id:
//!   {"id": "...", "result": {...}}  |  {"id": "...", "error": {"code", "message"}}
//! Pushed notifications (events) arrive as:
//!   {"event": "<type>", "data": {...}}
//!
//! Verified against herdr 0.7.5-preview (socket protocol v18, schema version 1),
//! preview build 2026-07-29-44b3adb12552. Canonical verification source:
//! `herdr api schema --json` (fields: protocol, schema_version).

/// The herdr socket protocol version this client is verified against —
/// single source of truth (bd-deps-protocol-check). `flywheel deps` warns
/// when the live server reports a different value; a bump that adds required
/// fields would break the client at runtime, and the WARN line turns a
/// confusing mid-loop failure into a five-second diagnosis.
pub const HDR_PROTOCOL: u64 = 18;

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
pub struct AgentSessionInfo {
    pub agent: String,
    pub kind: String,
    pub source: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: Option<AgentStatus>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub display_agent: Option<String>,
    #[serde(default)]
    pub focused: bool,
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub interactive_ready: bool,
    #[serde(default)]
    pub launch_pending: bool,
    pub name: Option<String>,
    pub pane_id: String,
    #[serde(default)]
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
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub label: Option<String>,
    #[serde(default)]
    pub pane_count: u64,
    #[serde(default)]
    pub tab_count: u64,
    #[serde(default)]
    pub agent_status: Option<AgentStatus>,
    pub cwd: Option<String>,
}

/// Resolve which of several same-label workspaces a project actually maps
/// to. Single match wins; with duplicates (a stale workspace left behind by
/// a dropped agent vs the live spawn — bd-nil), prefer the workspace holding
/// MANAGED agents (names starting with `prefix`); otherwise the first match.
/// Deterministic — no wall clock in the durable path.
pub fn resolve_workspace_id(matches: &[WorkspaceInfo], agents: &[AgentInfo], prefix: &str) -> String {
    if matches.len() == 1 {
        return matches[0].workspace_id.clone();
    }
    for ws in matches {
        if agents.iter().any(|a| {
            a.workspace_id == ws.workspace_id
                && a.name.as_deref().map(|n| n.starts_with(prefix)).unwrap_or(false)
        }) {
            return ws.workspace_id.clone();
        }
    }
    matches[0].workspace_id.clone()
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub agent_status: Option<AgentStatus>,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorktreeInfo {
    pub workspace_id: String,
    pub branch: Option<String>,
    pub path: Option<String>,
    pub label: Option<String>,
}

/// Push notification pushed by the server (event subscription or server events).
#[derive(Debug, Clone, Deserialize)]
pub struct PushEvent {
    #[serde(rename = "event")]
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneAgentStatusChanged {
    pub pane_id: String,
    pub workspace_id: String,
    pub agent_status: AgentStatus,
    pub agent: Option<String>,
    #[serde(default)]
    pub display_agent: Option<String>,
    pub title: Option<String>,
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
    pub fn call_coded(&mut self, method: &str, params: Value) -> std::result::Result<Value, HerdrError> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let req = json!({ "id": id, "method": method, "params": params });
        let mut line = serde_json::to_string(&req)
            .map_err(|e| HerdrError { code: "serialize".into(), message: e.to_string() })?;
        line.push('\n');

        let mut reader = BufReader::new(self.open().map_err(|e| HerdrError {
            code: "connect".into(),
            message: e.to_string(),
        })?);
        reader
            .get_mut()
            .write_all(line.as_bytes())
            .map_err(|e| HerdrError { code: "io".into(), message: format!("write to herdr socket: {e}") })?;
        reader
            .get_mut()
            .flush()
            .map_err(|e| HerdrError { code: "io".into(), message: format!("flush herdr socket: {e}") })?;

        loop {
            let mut buf = String::new();
            let n = reader
                .read_line(&mut buf)
                .map_err(|e| HerdrError { code: "io".into(), message: format!("read from herdr socket: {e}") })?;
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
            return val
                .get("result")
                .cloned()
                .ok_or_else(|| HerdrError {
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
                        err.get("message").and_then(Value::as_str).unwrap_or("unknown")
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
        let root: PaneInfo = serde_json::from_value(
            res.get("root_pane").cloned().unwrap_or(res.clone()),
        )
        .context("parse workspace.create root_pane")?;
        Ok((ws, root))
    }

    pub fn workspace_list(&mut self) -> Result<Vec<WorkspaceInfo>> {
        let res = self.call("workspace.list", json!({}))?;
        let ws: Vec<WorkspaceInfo> = serde_json::from_value(
            res.get("workspaces").cloned().unwrap_or(res),
        )
        .context("parse workspace.list result")?;
        Ok(ws)
    }

    pub fn workspace_close(&mut self, workspace_id: &str) -> Result<Value> {
        self.call("workspace.close", json!({ "workspace_id": workspace_id }))
    }

    pub fn pane_split(&mut self, pane_id: &str, direction: &str, cwd: Option<&str>) -> Result<PaneInfo> {
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
        self.call(
            "agent.rename",
            json!({ "target": target, "name": name }),
        )
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
        serde_json::from_value(res.get("agent").cloned().unwrap_or(res))
            .map_err(|e| HerdrError { code: "parse".into(), message: e.to_string() })
    }

    pub fn agent_wait(&mut self, target: &str, until: &[AgentStatus], timeout_ms: Option<u64>) -> Result<AgentInfo> {
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

    pub fn agent_read(
        &mut self,
        target: &str,
        source: &str,
        lines: Option<u32>,
    ) -> Result<String> {
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
                message: format!("unknown pane wait-output mode: {other} (expected \"match\" or \"regex\")"),
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
pub fn parse_wait_for_output_result(res: &Value) -> std::result::Result<PaneWaitResult, HerdrError> {
    let parse_err = |msg: String| HerdrError { code: "parse".into(), message: msg };
    if res.get("type").and_then(Value::as_str) != Some("output_matched") {
        return Err(parse_err(format!("unexpected pane.wait_for_output result: {res}")));
    }
    let pane_id = res
        .get("pane_id")
        .and_then(Value::as_str)
        .ok_or_else(|| parse_err(format!("pane.wait_for_output result missing pane_id: {res}")))?
        .to_string();
    let matched_line = res.get("matched_line").and_then(Value::as_str).map(str::to_string);
    let read = res
        .get("read")
        .and_then(|r| r.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| parse_err(format!("pane.wait_for_output result missing read.text: {res}")))?
        .to_string();
    Ok(PaneWaitResult { pane_id, matched_line, read })
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub kind: String,
    pub pane_id: Option<String>,
    pub workspace_id: Option<String>,
    pub agent_status: Option<AgentStatus>,
}

impl Subscription {
    pub fn to_value(&self) -> Value {
        json!({
            "type": self.kind,
            "pane_id": self.pane_id,
            "workspace_id": self.workspace_id,
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
            "interactive_ready": true,
            "revision": 3
        });
        let a: AgentInfo = serde_json::from_value(v).unwrap();
        assert_eq!(a.agent_status, Some(AgentStatus::Working));
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
        assert_eq!(e.kind, "pane.agent_status_changed");
        let s: PaneAgentStatusChanged = serde_json::from_value(e.data).unwrap();
        assert_eq!(s.agent_status, AgentStatus::Blocked);
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
        assert_eq!(r.matched_line.as_deref(), Some("❯ sleep 1; echo probe-done-123"));
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

    /// Wire verification against a live herdr server: needs a real pane id
    /// (env `PANE_ID`) whose terminal already contains the pattern — pane
    /// waits search existing output immediately (docs' caveat).
    ///   PANE_ID=w13:p1 cargo test -- --ignored pane_wait_output_live
    #[test]
    #[ignore]
    fn pane_wait_output_live() {
        let pane_id = std::env::var("PANE_ID").expect("set PANE_ID to a live herdr pane id");
        let mut c = Client::connect(None).unwrap();
        // match against output that already exists
        let r = c
            .pane_wait_output(&pane_id, "match", "flywheel", Some(5_000))
            .unwrap();
        assert_eq!(r.pane_id, pane_id);
        assert!(!r.read.is_empty());
        // non-matching pattern must ERROR (code "timeout"), not hang
        let e = c
            .pane_wait_output(&pane_id, "match", "THIS-WILL-NEVER-MATCH-xyzzy", Some(1_500))
            .unwrap_err();
        assert_eq!(e.code, "timeout");
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn parse_protocol_reads_top_level_field() {
        let raw = r#"{"protocol": 18, "schema_version": 1, "schemas": {}}"#;
        assert_eq!(parse_protocol(raw), Some(18));
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
            label: Some(label.to_string()),
            pane_count: 1,
            tab_count: 1,
            agent_status: None,
            cwd: None,
        }
    }

    fn agent(name: Option<&str>, ws_id: &str) -> AgentInfo {
        AgentInfo {
            agent: Some("pi".into()),
            agent_session: None,
            agent_status: Some(AgentStatus::Idle),
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
        assert_eq!(resolve_workspace_id(&matches, &agents, "flywheel-x-"), "w15");
    }

    #[test]
    fn duplicate_labels_both_dead_fall_back_to_first() {
        let matches = vec![ws("w14", "flywheel-x"), ws("w15", "flywheel-x")];
        let agents = vec![agent(Some("unrelated-oc"), "w15")];
        assert_eq!(resolve_workspace_id(&matches, &agents, "flywheel-x-"), "w14");
    }

    #[test]
    fn duplicate_labels_both_live_prefer_first_deterministically() {
        let matches = vec![ws("w14", "flywheel-x"), ws("w15", "flywheel-x")];
        let agents = vec![
            agent(Some("flywheel-x-oc"), "w14"),
            agent(Some("flywheel-x-oc2"), "w15"),
        ];
        assert_eq!(resolve_workspace_id(&matches, &agents, "flywheel-x-"), "w14");
    }

    #[test]
    fn nameless_managed_agent_does_not_count() {
        // a pi record without a name is not a managed-agent signal — the
        // rename fix (bd-w17) names it at spawn time
        let matches = vec![ws("w14", "flywheel-x"), ws("w15", "flywheel-x")];
        let agents = vec![agent(None, "w15")];
        assert_eq!(resolve_workspace_id(&matches, &agents, "flywheel-x-"), "w14");
    }
}
