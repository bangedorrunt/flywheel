//! agent-mail backend — a REAL `MailBackend` over the `mcp_agent_mail` server
//! (127.0.0.1:8765), driven by the one parameterized transport
//! (`McpClient` with `ServerKind::AgentMail`, ADR-0003 V1). NOT a toron
//! wrapper — every trait method maps to the agent-mail MCP tool of the same
//! name (per-tool argument mapping lives here, where AM diverges from toron).
//!
//! governed-by: ADR-0002, ADR-0003  ·  implements: `MailBackend`
//!
//! LAZY (ADR-0003 V4, BUG2): construction never dials — `new` only builds the
//! `McpClient`. `health()`/ops attempt the first call on demand and map a
//! transport failure (server down) onto an actionable error naming the URL and
//! the escape hatch: `agent-mail server not reachable at {url}; start am or
//! set FLYWHEEL_MAIL=toron`. No silent fallback (ADR-0002 §6).
//!
//! `health()` uses the MCP `health_check` TOOL (ADR-0003 §5 BUG4), not a REST
//! `/health` call. Pane-identity write/clear are local filesystem ops (the
//! agent-mail server exposes no write tool; it reads the files this backend
//! writes — the same `agent-mail/identity/<project_hash>/<pane_id>` layout
//! `mail_shim` uses).

use crate::backend::{BackendError, InboxFilter, MailBackend, Result, SearchFilter};
use crate::mcp_http::{McpClient, McpError, ServerKind};
use serde_json::{json, Value};

/// Real agent-mail backend. Constructed lazily by the factory; never dials on
/// construction (ADR-0003 V4).
pub struct AgentMailBackend {
    /// AM-targeting MCP client (ServerKind::AgentMail → truthful error labels).
    client: McpClient,
    /// The agent identity this backend sends AS / reads the inbox for.
    name: String,
    /// Base URL — used in the actionable not-reachable error.
    url: String,
}

impl AgentMailBackend {
    /// Build the backend against the agent-mail server at `url` (e.g.
    /// `http://127.0.0.1:8765`). LAZY: no dialing happens here.
    pub fn new(url: &str, token: Option<String>, name: String) -> Result<Self> {
        let client = McpClient::new(url, token, ServerKind::AgentMail)?;
        Ok(Self {
            client,
            name,
            url: url.to_string(),
        })
    }

    /// Call one agent-mail MCP tool. Transport failures (server down) map onto
    /// the actionable BUG2 error; tool-level errors stay truthful with the
    /// `agent-mail <tool>` label (BUG3).
    fn call(&self, tool: &str, args: Value) -> Result<Value> {
        self.client.call_tool(tool, args).map_err(|e| match e {
            McpError::Transport(t) => BackendError::Other(anyhow::anyhow!(
                "agent-mail server not reachable at {}; start am or set FLYWHEEL_MAIL=toron ({t})",
                self.url
            )),
            McpError::Tool(t) => BackendError::Other(anyhow::anyhow!("agent-mail {tool}: {t}")),
        })
    }
}

impl MailBackend for AgentMailBackend {
    fn name(&self) -> &'static str {
        "agentmail"
    }

    fn agent_name(&self) -> &str {
        &self.name
    }

    fn health(&mut self) -> Result<Value> {
        // BUG4: the MCP `health_check` tool, NOT a REST /health.
        self.call("health_check", json!({}))
    }

    fn ensure_project(&mut self, project_key: &str) -> Result<Value> {
        self.call("ensure_project", json!({ "human_key": project_key }))
    }

    fn register_agent(
        &mut self,
        project_key: &str,
        name: &str,
        program: &str,
        model: &str,
        task: &str,
    ) -> Result<Value> {
        self.call(
            "register_agent",
            json!({
                "project_key": project_key,
                "name": name,
                "program": program,
                "model": model,
                "task_description": task,
            }),
        )
    }

    fn list_agents(&mut self, project_key: &str) -> Result<Value> {
        self.call("list_agents", json!({ "project_key": project_key }))
    }

    fn send(
        &mut self,
        project_key: &str,
        to: &[String],
        subject: &str,
        body: &str,
        importance: &str,
        ack_required: bool,
        thread_id: Option<&str>,
    ) -> Result<Value> {
        let mut args = json!({
            "project_key": project_key,
            "sender_name": self.name,
            "to": to,
            "subject": subject,
            "body_md": body,
            "importance": importance,
            "ack_required": ack_required,
        });
        if let Some(tid) = thread_id {
            args["thread_id"] = json!(tid);
        }
        self.call("send_message", args)
    }

    fn reply(
        &mut self,
        project_key: &str,
        message_id: u64,
        body: &str,
        to: Option<&[String]>,
    ) -> Result<Value> {
        // AM's reply_message takes the numeric `message_id` (toron's takes a
        // hex thread_id — the per-backend mapping divergence of ADR-0003 V1).
        self.call(
            "reply_message",
            json!({
                "project_key": project_key,
                "sender_name": self.name,
                "to": to.map(|t| t.to_vec()).unwrap_or_default(),
                "subject": "",
                "body_md": body,
                "message_id": message_id as i64,
            }),
        )
    }

    fn inbox(&mut self, project_key: &str, agent: &str, filter: &InboxFilter) -> Result<Value> {
        let mut args = serde_json::Map::new();
        args.insert("project_key".into(), json!(project_key));
        args.insert("agent_name".into(), json!(agent));
        args.insert("urgent_only".into(), json!(filter.urgent_only));
        args.insert("unread_only".into(), json!(filter.unread_only));
        args.insert("ack_overdue_only".into(), json!(filter.ack_overdue_only));
        args.insert("include_bodies".into(), json!(filter.include_bodies));
        args.insert("limit".into(), json!(filter.limit));
        if let Some(ts) = &filter.since_ts {
            args.insert("since_ts".into(), json!(ts));
        }
        self.call("fetch_inbox", Value::Object(args))
    }

    fn ack(&mut self, project_key: &str, agent: &str, message_id: u64) -> Result<Value> {
        self.call(
            "acknowledge_message",
            json!({
                "project_key": project_key,
                "agent_name": agent,
                "message_id": message_id as i64,
            }),
        )
    }

    fn mark_read(&mut self, project_key: &str, agent: &str, message_id: u64) -> Result<Value> {
        self.call(
            "mark_message_read",
            json!({
                "project_key": project_key,
                "agent_name": agent,
                "message_id": message_id as i64,
            }),
        )
    }

    fn reserve(
        &mut self,
        project_key: &str,
        _agent: &str,
        paths: &[String],
        ttl_seconds: u64,
        exclusive: bool,
        reason: &str,
    ) -> Result<Value> {
        // AM registers reservations for the caller identity; flywheel always
        // acts as itself (the `_agent` param is toron's per-agent targeting).
        let _ = _agent;
        self.call(
            "file_reservation_paths",
            json!({
                "project_key": project_key,
                "agent_name": self.name,
                "paths": paths,
                "ttl_seconds": ttl_seconds as i64,
                "exclusive": exclusive,
                "reason": reason,
            }),
        )
    }

    fn release(&mut self, project_key: &str, _agent: &str) -> Result<Value> {
        let _ = _agent;
        self.call(
            "release_file_reservations",
            json!({ "project_key": project_key, "agent_name": self.name }),
        )
    }

    fn renew(
        &mut self,
        project_key: &str,
        agent: &str,
        extend_seconds: u64,
        paths: Option<&[String]>,
    ) -> Result<Value> {
        let mut args = json!({
            "project_key": project_key,
            "agent_name": agent,
            "extend_seconds": extend_seconds as i64,
        });
        if let Some(p) = paths {
            args["paths"] = json!(p);
        }
        // AM already returns the `file_reservations` envelope consumers read.
        self.call("renew_file_reservations", args)
    }

    fn conflicts(&mut self, project_key: &str, _agent: &str, paths: &[String]) -> Result<Value> {
        let _ = _agent;
        self.call(
            "check_file_reservation_conflicts",
            json!({
                "project_key": project_key,
                "agent_name": self.name,
                "paths": paths,
            }),
        )
    }

    fn resolve_pane_identity(&mut self, project_key: &str, pane_id: &str) -> Result<Value> {
        self.call(
            "resolve_pane_identity",
            json!({ "project_key": project_key, "pane_id": pane_id }),
        )
    }

    fn write_pane_identity(
        &mut self,
        project_key: &str,
        pane_id: &str,
        agent_name: &str,
    ) -> Result<Value> {
        // Local filesystem op — the agent-mail server reads the identity files
        // this writes (it exposes no write tool; same layout as agent-mail
        // core). ADR-0001 A4 / bd-mail-pane-identity-bridge.
        let path = crate::mail_shim::write_identity_for(project_key, pane_id, agent_name)
            .map_err(BackendError::Other)?;
        Ok(json!({ "path": path.to_string_lossy() }))
    }

    fn clear_pane_identity(&mut self, project_key: &str, pane_id: &str) -> Result<Value> {
        let path = crate::mail_shim::clear_identity_for(project_key, pane_id)
            .map_err(BackendError::Other)?;
        Ok(json!({ "path": path.to_string_lossy() }))
    }

    fn slot_acquire(
        &mut self,
        project_key: &str,
        agent: &str,
        slot: &str,
        exclusive: bool,
        ttl_seconds: u64,
    ) -> Result<Value> {
        self.call(
            "acquire_build_slot",
            json!({
                "project_key": project_key,
                "agent_name": agent,
                "slot": slot,
                "exclusive": exclusive,
                "ttl_seconds": ttl_seconds as i64,
            }),
        )
    }

    fn slot_release(&mut self, project_key: &str, agent: &str, slot: &str) -> Result<Value> {
        self.call(
            "release_build_slot",
            json!({
                "project_key": project_key,
                "agent_name": agent,
                "slot": slot,
            }),
        )
    }

    fn search(
        &mut self,
        project_key: &str,
        query: &str,
        limit: u32,
        filter: &SearchFilter,
    ) -> Result<Value> {
        let mut args = serde_json::Map::new();
        args.insert("project_key".into(), json!(project_key));
        args.insert("query".into(), json!(query));
        args.insert("limit".into(), json!(limit));
        if filter.include_body_md {
            args.insert("include_body_md".into(), json!(true));
        }
        if let Some(s) = &filter.sender {
            args.insert("sender".into(), json!(s));
        }
        if let Some(i) = &filter.importance {
            args.insert("importance".into(), json!(i));
        }
        if let Some(t) = &filter.thread_id {
            args.insert("thread_id".into(), json!(t));
        }
        if let Some(d) = &filter.date_start {
            args.insert("date_start".into(), json!(d));
        }
        if let Some(d) = &filter.date_end {
            args.insert("date_end".into(), json!(d));
        }
        if let Some(r) = &filter.ranking {
            args.insert("ranking".into(), json!(r));
        }
        self.call("search_messages", Value::Object(args))
    }

    fn summarize_thread(
        &mut self,
        project_key: &str,
        thread_id: &str,
        llm_mode: bool,
    ) -> Result<Value> {
        self.call(
            "summarize_thread",
            json!({
                "project_key": project_key,
                "thread_id": thread_id,
                "llm_mode": llm_mode,
            }),
        )
    }

    fn request_contact(
        &mut self,
        project_key: &str,
        from_agent: &str,
        to_agent: &str,
        to_project: Option<&str>,
        reason: &str,
    ) -> Result<Value> {
        let mut args = json!({
            "project_key": project_key,
            "from_agent": from_agent,
            "to_agent": to_agent,
            "reason": reason,
        });
        if let Some(tp) = to_project {
            args["to_project"] = json!(tp);
        }
        self.call("request_contact", args)
    }

    fn respond_contact(
        &mut self,
        project_key: &str,
        to_agent: &str,
        from_agent: &str,
        accept: bool,
    ) -> Result<Value> {
        self.call(
            "respond_contact",
            json!({
                "project_key": project_key,
                "to_agent": to_agent,
                "from_agent": from_agent,
                "accept": accept,
            }),
        )
    }

    fn list_contacts(&mut self, project_key: &str, agent: &str) -> Result<Value> {
        self.call(
            "list_contacts",
            json!({ "project_key": project_key, "agent_name": agent }),
        )
    }

    fn set_contact_policy(
        &mut self,
        project_key: &str,
        agent: &str,
        policy: &str,
    ) -> Result<Value> {
        self.call(
            "set_contact_policy",
            json!({
                "project_key": project_key,
                "agent_name": agent,
                "policy": policy,
            }),
        )
    }

    fn ensure_product(&mut self, product_key: &str) -> Result<Value> {
        self.call("ensure_product", json!({ "product_key": product_key }))
    }

    fn products_link(&mut self, product_key: &str, project_key: &str) -> Result<Value> {
        self.call(
            "products_link",
            json!({ "product_key": product_key, "project_key": project_key }),
        )
    }

    fn search_product(&mut self, product_key: &str, query: &str, limit: u32) -> Result<Value> {
        self.call(
            "search_messages_product",
            json!({
                "product_key": product_key,
                "query": query,
                "limit": limit,
                "include_body_md": true,
            }),
        )
    }

    fn inbox_product(&mut self, product_key: &str, agent: &str, limit: u32) -> Result<Value> {
        self.call(
            "fetch_inbox_product",
            json!({
                "product_key": product_key,
                "agent_name": agent,
                "limit": limit,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MailBackend;

    /// LAZY construct: `new` must NOT dial (ADR-0003 V4). Pointing at a dead
    /// port proves construction succeeds without any network activity.
    #[test]
    fn constructs_without_dialing() {
        let b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into())
            .expect("lazy construct must not dial");
        assert_eq!(b.name(), "agentmail");
        assert_eq!(b.agent_name(), "Orchid");
    }

    /// The default build's backend label is truthfully "agentmail".
    #[test]
    fn name_is_agentmail() {
        let b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into()).unwrap();
        assert_eq!(b.name(), "agentmail");
    }

    /// BUG2: an unreachable server yields the actionable error — naming the
    /// URL and the escape hatch (`FLYWHEEL_MAIL=toron`). Port 1 on loopback is
    /// never listening, so the first call fails fast with a transport error.
    #[test]
    fn unreachable_server_returns_actionable_error() {
        let mut b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into()).unwrap();
        match b.health() {
            Err(BackendError::Other(e)) => {
                let s = format!("{e:#}");
                assert!(
                    s.contains("agent-mail server not reachable at http://127.0.0.1:1"),
                    "names the url: {s}"
                );
                assert!(
                    s.contains("FLYWHEEL_MAIL=toron"),
                    "points at the escape hatch: {s}"
                );
            }
            Err(e) => panic!("expected Other (transport), got {e:?}"),
            Ok(_) => panic!("health cannot succeed against a dead port"),
        }
    }

    /// The transport error string is truthful to the backend label (BUG3):
    /// the agent-mail backend never says "toron".
    #[test]
    fn error_strings_say_agent_mail_not_toron() {
        let mut b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into()).unwrap();
        match b.ensure_project("p") {
            Err(BackendError::Other(e)) => {
                let s = format!("{e:#}");
                assert!(s.starts_with("agent-mail"), "labels the backend: {s}");
                assert!(!s.contains("toron MCP"), "no toron-label leak: {s}");
            }
            Err(e) => panic!("expected Other, got {e:?}"),
            Ok(_) => panic!("cannot succeed against a dead port"),
        }
    }

    /// Pane-identity write is a LOCAL filesystem op (no dialing): it succeeds
    /// even against a dead server, writing the canonical identity file.
    #[test]
    fn write_pane_identity_is_local_not_network() {
        let mut b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into()).unwrap();
        // temp project path + pane id that sanitizes to a file-safe name
        let key = std::env::temp_dir().join("fw-am-identity-test");
        let key = key.to_string_lossy().to_string();
        let v = b
            .write_pane_identity(&key, "w1:p1", "Orchid")
            .expect("local write succeeds without a server");
        assert!(v.get("path").is_some(), "returns the written path: {v}");
        let path = v["path"].as_str().unwrap();
        assert!(std::path::Path::new(path).exists(), "file exists at {path}");
        let _ = b.clear_pane_identity(&key, "w1:p1").expect("local clear");
        assert!(
            !std::path::Path::new(path).exists(),
            "file removed after clear"
        );
    }
}
