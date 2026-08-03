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
        let id = self.next_id.to_string();
        self.next_id += 1;
        let req = json!({ "id": id, "method": method, "params": params });
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');

        let mut reader = BufReader::new(self.open()?);
        reader
            .get_mut()
            .write_all(line.as_bytes())
            .context("write to herdr socket")?;
        reader.get_mut().flush().context("flush herdr socket")?;

        loop {
            let mut buf = String::new();
            let n = reader
                .read_line(&mut buf)
                .context("read from herdr socket")?;
            if n == 0 {
                bail!("herdr socket closed while waiting for response");
            }
            let val: Value = serde_json::from_str(buf.trim())
                .with_context(|| format!("malformed herdr response: {buf}"))?;
            // ignore pushed notifications (no matching id) and other ids
            if val.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                continue;
            }
            if let Some(err) = val.get("error") {
                let code = err
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                bail!("herdr error {code}: {msg}");
            }
            return val
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("herdr response missing result: {val}"));
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

    pub fn agent_prompt(
        &mut self,
        target: &str,
        text: &str,
        wait: Option<WaitOptions>,
    ) -> Result<AgentInfo> {
        let wait = wait.map(|w| {
            json!({ "timeout_ms": w.timeout_ms, "until": w.until.iter().map(|s| s.as_str()).collect::<Vec<_>>() })
        });
        let res = self.call(
            "agent.prompt",
            json!({ "target": target, "text": text, "wait": wait }),
        )?;
        serde_json::from_value(res.get("agent").cloned().unwrap_or(res))
            .context("parse agent.prompt result")
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
}
