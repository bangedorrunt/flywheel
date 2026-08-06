//! Minimal MCP-over-Streamable-HTTP client (protocol 2026-07-28, stateless).
//!
//! Used by the remote-daemon mail backend (ADR-0003 Decision 3B, bd-o8j.3):
//! flywheel POSTs JSON-RPC to a standalone `toron serve --mcp-http <addr>`
//! daemon's `/mcp` endpoint and drives the toron tool surface. No rmcp dep —
//! the wire contract below was verified byte-for-byte against the live daemon
//! on 2026-08-06.
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
//! governed-by: ADR-0003

use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Protocol version spoken by the client (toron's rmcp server prefers it).
const PROTOCOL_VERSION: &str = "2026-07-28";

/// Minimal MCP-over-HTTP client bound to one daemon endpoint. The methods are
/// synchronous (`ureq`), matching flywheel's sync dispatch; `call_tool` runs
/// the `initialize` handshake once on first use (stateless mode accepts
/// requests without it, but the handshake is part of the MCP contract).
pub struct McpClient {
    /// `{base}/mcp` — the Streamable HTTP mount toron serves.
    endpoint: String,
    /// Optional bearer token (`Authorization: Bearer …`).
    token: Option<String>,
    /// Lazily-runs `initialize` once per client (per daemon endpoint).
    initialized: OnceLock<()>,
    /// Monotonic JSON-RPC id.
    next_id: std::sync::atomic::AtomicU64,
}

impl McpClient {
    /// Build a client for a daemon at `base_url` (e.g. `http://127.0.0.1:19001`).
    pub fn new(base_url: &str, token: Option<String>) -> Result<Self> {
        let base = base_url.trim_end_matches('/');
        if base.is_empty() {
            bail!("agent-mail url is empty");
        }
        Ok(Self {
            endpoint: format!("{base}/mcp"),
            token,
            initialized: OnceLock::new(),
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// Call a tool on the daemon; returns the tool's structured JSON (parsed
    /// from the text ContentBlock toron emits).
    pub fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        if self.initialized.get().is_none() {
            self.initialize()?;
            let _ = self.initialized.set(());
        }
        let result = self.post("tools/call", json!({ "name": name, "arguments": arguments }))?;
        Self::parse_tool_result(name, &result)
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
        self.post("initialize", Value::Object(params)).map(|_| ())
    }

    /// Extract the tool's structured JSON from a `tools/call` result.
    fn parse_tool_result(name: &str, result: &Value) -> Result<Value> {
        if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
            let text = result
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or("tool error (no detail)");
            bail!("toron {name} failed: {text}");
        }
        // SEP-2106 structured content, when the server emits it.
        if let Some(sc) = result.get("structuredContent") {
            return Ok(sc.clone());
        }
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .with_context(|| format!("toron {name}: tool result has no text content: {result}"))?;
        serde_json::from_str(text).with_context(|| format!("toron {name}: result text is not JSON"))
    }

    /// POST one JSON-RPC request; returns the `result` field of the response.
    fn post(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            let name = map
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            req = req.set("Mcp-Method", "tools/call").set("Mcp-Name", name);
        }

        let resp = match req.send_json(&body) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                bail!("toron MCP {method} HTTP {code}: {text}");
            }
            Err(e) => bail!("toron MCP {method}: {e}"),
        };
        if !(200..300).contains(&resp.status()) {
            bail!("toron MCP {method} HTTP {}", resp.status());
        }
        let value: Value = resp
            .into_json()
            .with_context(|| format!("toron MCP {method}: non-JSON response"))?;

        if let Some(err) = value.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("MCP error");
            bail!("toron MCP {method} error ({code}): {msg}");
        }
        value
            .get("result")
            .cloned()
            .with_context(|| format!("toron MCP {method}: no result in response"))
    }
}
