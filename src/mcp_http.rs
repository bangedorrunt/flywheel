//! Minimal MCP-over-Streamable-HTTP client (protocol 2026-07-28, stateless).
//!
//! Used by BOTH remote mail backends (ADR-0003 V1): flywheel POSTs JSON-RPC to
//! a server's `/mcp` endpoint and drives that server's tool surface. One
//! parameterized transport serves the real agent-mail server (`ServerKind::AgentMail`)
//! and the toron daemon (`ServerKind::Toron`) — the label is what differs (it
//! names error strings truthfully). No rmcp dep — the wire contract below was
//! verified byte-for-byte against the live daemon on 2026-08-06 (the strict
//! toron envelope returns 200 + correct `health_check` on agent-mail too).
//!
//! Wire contract (2026-07-28 stateless, SEP-2567):
//! - every POST carries `MCP-Protocol-Version: 2026-07-28`;
//! - the request body carries `_meta` INSIDE `params` (SEP-1319/SEP-2575
//!   RequestMetaObject) with the namespaced keys
//!   `io.modelcontextprotocol/{protocolVersion,clientCapabilities,clientInfo}`;
//! - `tools/call` additionally requires the SEP-2243 standard headers
//!   `Mcp-Method: tools/call` and `Mcp-Name: <tool>`;
//! - the tool's structured JSON rides in `result.content[0].text` (toron
//!   emits a text ContentBlock, not `structuredContent`); `result.isError`
//!   carries tool-level failures.
//!
//! Lazy by construction: `McpClient::new` builds the endpoint string only and
//! never dials; the `initialize` handshake runs once on first `call_tool`.
//!
//! governed-by: ADR-0003

use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Protocol version spoken by the client (toron's rmcp server prefers it).
const PROTOCOL_VERSION: &str = "2026-07-28";

/// Which server a client targets. Drives error-string labels only — the wire
/// envelope is identical (ADR-0003 V1: one transport, the label is what
/// differs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    /// The real mcp_agent_mail server (default `http://127.0.0.1:8765`).
    AgentMail,
    /// The toron daemon (default `http://127.0.0.1:18080`).
    Toron,
}

impl ServerKind {
    /// The truthful backend label used in error strings (BUG3 fix).
    pub fn label(self) -> &'static str {
        match self {
            ServerKind::AgentMail => "agent-mail",
            ServerKind::Toron => "toron",
        }
    }
}

/// Transport-vs-tool error distinction, so backends can give actionable
/// guidance when a server is unreachable (ADR-0003 V4) without conflating a
/// genuine tool failure.
#[derive(Debug)]
pub enum McpError {
    /// The server could not be reached (connection refused / timeout / DNS).
    Transport(anyhow::Error),
    /// The server responded but rejected the request (HTTP status, JSON-RPC
    /// error, or a tool-level `isError`).
    Tool(anyhow::Error),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Transport(e) => write!(f, "transport: {e}"),
            McpError::Tool(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for McpError {}

/// Minimal MCP-over-HTTP client bound to one server endpoint. The methods are
/// synchronous (`ureq`), matching flywheel's sync dispatch; `call_tool` runs
/// the `initialize` handshake once on first use (stateless mode accepts
/// requests without it, but the handshake is part of the MCP contract).
pub struct McpClient {
    /// `{base}/mcp` — the Streamable HTTP mount both servers serve.
    endpoint: String,
    /// Optional bearer token (`Authorization: Bearer …`).
    token: Option<String>,
    /// Which server this client targets (labels error strings).
    kind: ServerKind,
    /// Lazily-runs `initialize` once per client (per server endpoint).
    initialized: OnceLock<()>,
    /// Monotonic JSON-RPC id.
    next_id: std::sync::atomic::AtomicU64,
}

impl McpClient {
    /// Build a client for a server at `base_url` (e.g. `http://127.0.0.1:8765`
    /// or `http://127.0.0.1:18080`). Lazy: constructs the endpoint string only,
    /// never dials. A base already ending in `/mcp` is accepted as-is (no
    /// double-append — legacy configs carried the `/mcp` suffix).
    pub fn new(base_url: &str, token: Option<String>, kind: ServerKind) -> Result<Self> {
        let base = base_url.trim_end_matches('/');
        if base.is_empty() {
            bail!("{} url is empty", kind.label());
        }
        let endpoint = if base.ends_with("/mcp") {
            base.to_string()
        } else {
            format!("{base}/mcp")
        };
        Ok(Self {
            endpoint,
            token,
            kind,
            initialized: OnceLock::new(),
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// Call a tool on the server; returns the tool's structured JSON (parsed
    /// from the text ContentBlock the server emits). `Transport` errors mean
    /// the server was unreachable; `Tool` errors mean it responded in error.
    pub fn call_tool(&self, name: &str, arguments: Value) -> std::result::Result<Value, McpError> {
        if self.initialized.get().is_none() {
            self.initialize().map_err(McpError::Transport)?;
            let _ = self.initialized.set(());
        }
        let result = self.post(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )?;
        Self::parse_tool_result(self.kind.label(), name, &result).map_err(McpError::Tool)
    }

    /// Run the `initialize` handshake (result is ignored — stateless mode).
    fn initialize(&self) -> Result<()> {
        let mut params = serde_json::Map::new();
        params.insert("protocolVersion".into(), json!(PROTOCOL_VERSION));
        params.insert("capabilities".into(), json!({}));
        params.insert(
            "clientInfo".into(),
            json!({ "name": "flywheel", "version": env!("CARGO_PKG_VERSION") }),
        );
        self.post("initialize", Value::Object(params))
            .map(|_| ())
            .map_err(|e| e.into())
    }

    /// Extract the tool's structured JSON from a `tools/call` result.
    fn parse_tool_result(label: &str, name: &str, result: &Value) -> Result<Value> {
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let text = result
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or("tool error (no detail)");
            bail!("{label} {name} failed: {text}");
        }
        // SEP-2106 structured content, when the server emits it.
        if let Some(sc) = result.get("structuredContent") {
            return Ok(sc.clone());
        }
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .with_context(|| {
                format!("{label} {name}: tool result has no text content: {result}")
            })?;
        serde_json::from_str(text)
            .with_context(|| format!("{label} {name}: result text is not JSON"))
    }

    /// POST one JSON-RPC request; returns the `result` field of the response.
    fn post(&self, method: &str, params: Value) -> std::result::Result<Value, McpError> {
        let label = self.kind.label();
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // `_meta` is required INSIDE params for 2026-07-28 (SEP-2575).
        let mut map = params.as_object().cloned().unwrap_or_default();
        map.insert(
            "_meta".into(),
            json!({
                "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": { "name": "flywheel", "version": env!("CARGO_PKG_VERSION") },
            }),
        );
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": map });

        let mut req = ureq::post(&self.endpoint)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("MCP-Protocol-Version", PROTOCOL_VERSION);
        if let Some(token) = &self.token {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        if method == "tools/call" {
            let name = map.get("name").and_then(Value::as_str).unwrap_or_default();
            req = req.set("Mcp-Method", "tools/call").set("Mcp-Name", name);
        }

        let resp = match req.send_json(&body) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                return Err(McpError::Tool(anyhow::anyhow!(
                    "{label} MCP {method} HTTP {code}: {text}"
                )));
            }
            Err(e) => {
                return Err(McpError::Transport(anyhow::anyhow!(
                    "{label} MCP {method}: {e}"
                )));
            }
        };
        if !(200..300).contains(&resp.status()) {
            return Err(McpError::Tool(anyhow::anyhow!(
                "{label} MCP {method} HTTP {}",
                resp.status()
            )));
        }
        let value: Value = resp
            .into_json()
            .map_err(|e| McpError::Tool(anyhow::anyhow!("{label} MCP {method}: non-JSON: {e}")))?;

        if let Some(err) = value.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("MCP error");
            return Err(McpError::Tool(anyhow::anyhow!(
                "{label} MCP {method} error ({code}): {msg}"
            )));
        }
        value.get("result").cloned().map(Ok).unwrap_or_else(|| {
            Err(McpError::Tool(anyhow::anyhow!(
                "{label} MCP {method}: no result in response"
            )))
        })
    }
}
