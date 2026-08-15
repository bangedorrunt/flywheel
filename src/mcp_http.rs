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

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

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
#[derive(Debug)]
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

impl Clone for McpClient {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            token: self.token.clone(),
            kind: self.kind,
            initialized: OnceLock::new(),
            next_id: std::sync::atomic::AtomicU64::new(
                self.next_id.load(std::sync::atomic::Ordering::Relaxed),
            ),
        }
    }
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
            self.initialize()?;
            let _ = self.initialized.set(());
        }
        let result = self.post(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )?;
        Self::parse_tool_result(self.kind.label(), name, &result).map_err(McpError::Tool)
    }

    /// Run the `initialize` handshake (result is ignored — stateless mode).
    ///
    /// Propagates `post`'s `McpError` verbatim so a server that ANSWERS the
    /// handshake with an error stays a `Tool` error instead of being relabeled
    /// `Transport` (ADR-0003 V4: Transport = unreachable, Tool = responded in
    /// error — conflating them sends operators chasing the wrong daemon).
    fn initialize(&self) -> std::result::Result<(), McpError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    /// Serve one HTTP response per accepted connection. `respond` maps a
    /// request body to the raw HTTP response bytes; the listener thread
    /// accepts connections until the test drops the listener. The full
    /// request (headers + Content-Length body) is read before responding so
    /// the handshake is deterministic even when the client's request arrives
    /// in multiple TCP segments under parallel test load.
    fn serve_raw(respond: impl Fn(&[u8]) -> Vec<u8> + Send + 'static) -> String {
        use std::io::Read as _;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read until headers end AND the advertised body is complete.
                loop {
                    match s.read(&mut chunk) {
                        Ok(0) => break, // peer closed: respond with what we have
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                    let head_done = buf.windows(4).any(|w| w == b"\r\n\r\n");
                    if head_done {
                        match content_length(&buf) {
                            // Body complete once we have Content-Length bytes.
                            Some(need) => {
                                let head_end =
                                    buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
                                if buf.len() >= head_end + 4 + need {
                                    break;
                                }
                            }
                            // Headers done, no length header (ureq always sends
                            // Content-Length for JSON) — don't spin.
                            None => break,
                        }
                    }
                }
                let _ = s.write_all(&respond(&buf));
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// Content-Length of the HTTP request in `buf`, if the header is present.
    fn content_length(buf: &[u8]) -> Option<usize> {
        let head = std::str::from_utf8(buf).ok()?;
        let head_end = head.find("\r\n\r\n")?;
        head[..head_end].lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse().ok()
            } else {
                None
            }
        })
    }

    fn http(status_line: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    #[test]
    fn initialize_http_error_stays_tool_not_transport() {
        // A server that RESPONDS to the handshake with HTTP 400 is reachable —
        // it rejected the request. Must classify as Tool (ADR-0003 V4), never
        // as Transport ("server not reachable").
        let base = serve_raw(|_| {
            http(
                "400 Bad Request",
                r#"{"jsonrpc":"2.0","error":{"code":-32600,"message":"bad protocol"}}"#,
            )
        });
        let client = McpClient::new(&base, None, ServerKind::AgentMail).expect("client");
        let err = client.call_tool("health_check", json!({})).unwrap_err();
        assert!(
            matches!(err, McpError::Tool(_)),
            "handshake rejection must be Tool, got: {err}"
        );
        let msg = err.to_string();
        assert!(
            !msg.contains("transport"),
            "Tool error must not be labeled transport: {msg}"
        );
    }

    #[test]
    fn refused_connection_is_transport() {
        // Bind, grab the port, drop the listener → connection refused.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let client = McpClient::new(&format!("http://127.0.0.1:{port}"), None, ServerKind::Toron)
            .expect("client");
        let err = client.call_tool("health_check", json!({})).unwrap_err();
        assert!(
            matches!(err, McpError::Transport(_)),
            "refused connection must be Transport, got: {err}"
        );
    }

    #[test]
    fn healthy_server_handshakes_once_then_calls_tool() {
        // Full happy path: initialize 200 → tools/call 200 with a text
        // ContentBlock carrying the structured JSON. The initialized
        // OnceLock must make the handshake run exactly once.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let inits = Arc::new(AtomicUsize::new(0));
        let inits_closure = Arc::clone(&inits);
        let base = serve_raw(move |req| {
            let req = String::from_utf8_lossy(req);
            if req.contains("\"initialize\"") {
                inits_closure.fetch_add(1, Ordering::SeqCst);
                http(
                    "200 OK",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2026-07-28","capabilities":{}}}"#,
                )
            } else {
                http(
                    "200 OK",
                    r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"ok\":true}"}]}}"#,
                )
            }
        });
        let client = McpClient::new(&base, None, ServerKind::AgentMail).expect("client");
        let out = client
            .call_tool("health_check", json!({}))
            .expect("tool call succeeds");
        assert_eq!(out["ok"], true, "structured result parsed: {out}");
        // Second call must NOT re-handshake (OnceLock).
        let out2 = client
            .call_tool("health_check", json!({}))
            .expect("second tool call succeeds");
        assert_eq!(out2["ok"], true);
        assert_eq!(inits.load(Ordering::SeqCst), 1, "initialize runs once");
    }
}
