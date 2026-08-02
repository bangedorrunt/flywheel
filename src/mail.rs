//! agent-mail coordination client — JSON-RPC over HTTP against the MCP endpoint.
//! Server: http://127.0.0.1:8765/mcp/ (bearer token from config). Tools return
//! JSON-serialized text content; we deserialize the inner JSON.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

use crate::config::MailConfig;

/// Bound every HTTP call — a hanging agent-mail server must not stall the
/// loop (the durable `mail` path blocks on this client).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

fn post(url: &str, token: Option<&str>, body: Value) -> Result<Value> {
    let mut req = ureq::post(url)
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/json");
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = req
        .send_json(body)
        .map_err(|e| anyhow!("agent-mail request failed: {e}"))?;
    let out: Value = resp
        .into_json()
        .context("agent-mail returned non-JSON")?;
    if let Some(err) = out.get("error") {
        bail!(
            "agent-mail error: {}",
            err.get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
    }
    Ok(out)
}

/// Extract the JSON value inside an MCP tool result's first text content.
fn tool_result(resp: Value) -> Result<Value> {
    let content = resp
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("agent-mail response missing content: {resp}"))?;
    for c in content {
        if let Some(text) = c.get("text").and_then(Value::as_str) {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            // tool output is JSON text; fall back to raw string if not JSON
            return Ok(serde_json::from_str(trimmed).unwrap_or_else(|_| json!(trimmed)));
        }
    }
    bail!("agent-mail response has no text content")
}

pub struct Mail {
    pub base: String,
    pub token: Option<String>,
    pub name: String,
    next_id: u64,
}

impl Mail {
    pub fn from_config(cfg: Option<&MailConfig>) -> Result<Option<Self>> {
        let Some(c) = cfg else { return Ok(None) };
        let url = c.url.clone().ok_or_else(|| anyhow!("agent_mail.url unset"))?;
        let name = c
            .name
            .clone()
            .unwrap_or_else(|| "flywheel".to_string());
        Ok(Some(Mail {
            base: url,
            token: c.token.clone(),
            name,
            next_id: 1,
        }))
    }

    fn call(&mut self, tool: &str, args: Value) -> Result<Value> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let body = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        });
        let resp = post(&format!("{}/mcp/", self.base.trim_end_matches('/')), self.token.as_deref(), body)?;
        tool_result(resp)
    }

    /// GET /health on the server base (not an MCP tool).
    pub fn health(&mut self) -> Result<Value> {
        let url = format!("{}/health", self.base.trim_end_matches('/'));
        let mut req = ureq::get(&url).timeout(HTTP_TIMEOUT);
        if let Some(t) = self.token.as_deref() {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let resp = req
            .call()
            .map_err(|e| anyhow!("agent-mail health request failed: {e}"))?;
        resp.into_json()
            .context("agent-mail /health returned non-JSON")
    }

    pub fn ensure_project(&mut self, project_key: &str) -> Result<Value> {
        self.call("ensure_project", json!({ "human_key": project_key }))
    }

    pub fn list_agents(&mut self, project_key: &str) -> Result<Value> {
        self.call("list_agents", json!({ "project_key": project_key }))
    }

    pub fn inbox(&mut self, project_key: &str, agent_name: &str, urgent_only: bool, limit: u32) -> Result<Value> {
        self.call(
            "fetch_inbox",
            json!({
                "project_key": project_key, "agent_name": agent_name,
                "urgent_only": urgent_only, "limit": limit
            }),
        )
    }

    pub fn send(
        &mut self,
        project_key: &str,
        to: &[String],
        subject: &str,
        body: &str,
        importance: &str,
        ack: bool,
    ) -> Result<Value> {
        self.call(
            "send_message",
            json!({
                "project_key": project_key, "sender_name": self.name, "to": to,
                "subject": subject, "body_md": body, "importance": importance,
                "ack_required": ack
            }),
        )
    }

    pub fn reserve(&mut self, project_key: &str, paths: &[String], ttl_seconds: u64, exclusive: bool, reason: &str) -> Result<Value> {
        self.call(
            "file_reservation_paths",
            json!({
                "project_key": project_key, "agent_name": self.name, "paths": paths,
                "ttl_seconds": ttl_seconds, "exclusive": exclusive, "reason": reason
            }),
        )
    }

    pub fn release(&mut self, project_key: &str) -> Result<Value> {
        self.call(
            "release_file_reservations",
            json!({ "project_key": project_key, "agent_name": self.name }),
        )
    }

    pub fn conflicts(&mut self, project_key: &str, paths: &[String]) -> Result<Value> {
        self.call(
            "check_file_reservation_conflicts",
            json!({ "project_key": project_key, "agent_name": self.name, "paths": paths }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_parses_text_json() {
        let resp = json!({
            "jsonrpc": "2.0", "id": "1",
            "result": { "content": [ { "type": "text", "text": "{\"ok\": true, \"n\": 3}" } ] }
        });
        let v = tool_result(resp).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["n"], 3);
    }

    #[test]
    fn tool_result_falls_back_to_raw_text() {
        let resp = json!({
            "jsonrpc": "2.0", "id": "1",
            "result": { "content": [ { "type": "text", "text": "hello world" } ] }
        });
        let v = tool_result(resp).unwrap();
        assert_eq!(v, json!("hello world"));
    }

    #[test]
    fn error_response_surfaces() {
        let resp = json!({
            "jsonrpc": "2.0", "id": "1",
            "error": { "code": -32000, "message": "boom" }
        });
        let err = tool_result(resp).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }
}
