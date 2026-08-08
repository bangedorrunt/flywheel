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

use crate::backend::wire;
use crate::backend::{BackendError, InboxFilter, MailBackend, Result, SearchFilter};
use crate::mcp_http::{McpClient, McpError, ServerKind};
use serde_json::{Value, json};

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

    /// c3b.5: resolve the `Re: <parent subject>` subject for a reply. The
    /// parent's subject is looked up from the project inbox (the message being
    /// replied to must be visible to this identity). Falls back to `""` when
    /// the parent cannot be found — the wire builder still carries the subject
    /// field, so the hardcoded-empty contract is gone.
    fn parent_reply_subject(&self, project_key: &str, id: u64) -> Result<String> {
        let rows = self.call(
            "fetch_inbox",
            wire::inbox_args(
                project_key,
                &self.name,
                false,
                false,
                false,
                false,
                500,
                None,
            ),
        )?;
        let parent = rows.as_array().and_then(|rs| {
            rs.iter().find(|r| {
                r["id"].as_u64() == Some(id)
                    || r["id"].as_str().and_then(|s| s.parse::<u64>().ok()) == Some(id)
            })
        });
        Ok(match parent.and_then(|r| r["subject"].as_str()) {
            Some(subj) if !subj.is_empty() => format!("Re: {subj}"),
            _ => String::new(),
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

    fn health(&self) -> Result<Value> {
        // BUG4: the MCP `health_check` tool, NOT a REST /health.
        self.call("health_check", wire::health_check_args())
    }

    fn ensure_project(&self, project_key: &str) -> Result<Value> {
        self.call("ensure_project", wire::ensure_project_args(project_key))
    }

    fn register_agent(
        &self,
        project_key: &str,
        name: &str,
        program: &str,
        model: &str,
        task: &str,
    ) -> Result<Value> {
        self.call(
            "register_agent",
            wire::register_agent_args(project_key, name, program, model, task),
        )
    }

    fn list_agents(&self, project_key: &str) -> Result<Value> {
        self.call("list_agents", wire::list_agents_args(project_key))
    }

    fn send(
        &self,
        project_key: &str,
        to: &[String],
        subject: &str,
        body: &str,
        importance: &str,
        ack_required: bool,
        thread_id: Option<&str>,
    ) -> Result<Value> {
        self.call(
            "send_message",
            wire::send_args(
                project_key,
                &self.name,
                to,
                subject,
                body,
                importance,
                ack_required,
                thread_id,
            ),
        )
    }

    fn reply(
        &self,
        project_key: &str,
        message_id: &str,
        body: &str,
        to: Option<&[String]>,
    ) -> Result<Value> {
        // AM's reply_message takes the numeric `message_id` (toron's takes a
        // hex thread_id — the per-backend mapping divergence of ADR-0003 V1).
        // A non-decimal id is a caller bug — Err loudly, never 0 (se2.2).
        let id = message_id.parse::<u64>().map_err(|e| {
            anyhow::anyhow!("agent-mail message_id {message_id:?} is not a decimal id: {e}")
        })?;
        self.call(
            "reply_message",
            wire::reply_args(
                project_key,
                &self.name,
                &to.map(|t| t.to_vec()).unwrap_or_default(),
                // c3b.5: reply subject populated from the parent (`Re: `) —
                // the caller owns parent lookup; the wire builder carries it
                // (was hardcoded `""`).
                &self.parent_reply_subject(project_key, id)?,
                body,
                "message_id",
                json!(id),
            ),
        )
    }

    fn inbox(&self, project_key: &str, agent: &str, filter: &InboxFilter) -> Result<Value> {
        self.call(
            "fetch_inbox",
            wire::inbox_args(
                project_key,
                agent,
                filter.urgent_only,
                filter.unread_only,
                filter.ack_overdue_only,
                filter.include_bodies,
                filter.limit,
                filter.since_ts.as_deref(),
            ),
        )
    }

    fn ack(&self, project_key: &str, agent: &str, message_id: &str) -> Result<Value> {
        let id = message_id.parse::<u64>().map_err(|e| {
            anyhow::anyhow!("agent-mail message_id {message_id:?} is not a decimal id: {e}")
        })?;
        self.call(
            "acknowledge_message",
            wire::ack_args(project_key, agent, json!(id)),
        )
    }

    fn mark_read(&self, project_key: &str, agent: &str, message_id: &str) -> Result<Value> {
        let id = message_id.parse::<u64>().map_err(|e| {
            anyhow::anyhow!("agent-mail message_id {message_id:?} is not a decimal id: {e}")
        })?;
        self.call(
            "mark_message_read",
            wire::mark_read_args(project_key, agent, json!(id)),
        )
    }

    fn reserve(
        &self,
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
            wire::reserve_args(
                project_key,
                &self.name,
                paths,
                ttl_seconds,
                exclusive,
                reason,
            ),
        )
    }

    fn release(&self, project_key: &str, _agent: &str) -> Result<Value> {
        let _ = _agent;
        self.call(
            "release_file_reservations",
            wire::release_args(project_key, &self.name),
        )
    }

    fn renew(
        &self,
        project_key: &str,
        agent: &str,
        extend_seconds: u64,
        paths: Option<&[String]>,
    ) -> Result<Value> {
        // AM already returns the `file_reservations` envelope consumers read.
        self.call(
            "renew_file_reservations",
            wire::renew_args(project_key, agent, extend_seconds, paths),
        )
    }

    fn force_release_file_reservation(
        &self,
        project_key: &str,
        agent: &str,
        file_reservation_id: u64,
        note: Option<&str>,
        notify_previous: bool,
    ) -> Result<Value> {
        self.call(
            "force_release_file_reservation",
            wire::force_release_args(
                project_key,
                agent,
                json!(file_reservation_id),
                note,
                notify_previous,
            ),
        )
    }

    fn conflicts(&self, project_key: &str, _agent: &str, paths: &[String]) -> Result<Value> {
        let _ = _agent;
        self.call(
            "check_file_reservation_conflicts",
            wire::conflicts_args(project_key, &self.name, paths),
        )
    }

    fn resolve_pane_identity(&self, project_key: &str, pane_id: &str) -> Result<Value> {
        self.call(
            "resolve_pane_identity",
            wire::resolve_pane_identity_args(project_key, pane_id),
        )
    }

    fn write_pane_identity(
        &self,
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

    fn clear_pane_identity(&self, project_key: &str, pane_id: &str) -> Result<Value> {
        let path = crate::mail_shim::clear_identity_for(project_key, pane_id)
            .map_err(BackendError::Other)?;
        Ok(json!({ "path": path.to_string_lossy() }))
    }

    fn cleanup_pane_identities(&self, project_key: &str) -> Result<Value> {
        self.call(
            "cleanup_pane_identities",
            wire::cleanup_pane_identities_args(project_key),
        )
    }

    fn slot_acquire(
        &self,
        project_key: &str,
        agent: &str,
        slot: &str,
        exclusive: bool,
        ttl_seconds: u64,
    ) -> Result<Value> {
        self.call(
            "acquire_build_slot",
            wire::slot_acquire_args(project_key, agent, slot, exclusive, ttl_seconds),
        )
    }

    fn slot_release(&self, project_key: &str, agent: &str, slot: &str) -> Result<Value> {
        self.call(
            "release_build_slot",
            wire::slot_release_args(project_key, agent, slot),
        )
    }

    fn slot_renew(
        &self,
        project_key: &str,
        agent: &str,
        slot: &str,
        extend_seconds: u64,
    ) -> Result<Value> {
        self.call(
            "renew_build_slot",
            wire::slot_renew_args(project_key, agent, slot, extend_seconds),
        )
    }

    fn search(
        &self,
        project_key: &str,
        query: &str,
        limit: u32,
        filter: &SearchFilter,
    ) -> Result<Value> {
        self.call(
            "search_messages",
            wire::search_args(
                project_key,
                query,
                limit,
                filter.include_body_md,
                filter.sender.as_deref(),
                filter.importance.as_deref(),
                filter.thread_id.as_deref(),
                filter.date_start.as_deref(),
                filter.date_end.as_deref(),
                filter.ranking.as_deref(),
            ),
        )
    }

    fn summarize_thread(
        &self,
        project_key: &str,
        thread_id: &str,
        llm_mode: bool,
    ) -> Result<Value> {
        self.call(
            "summarize_thread",
            wire::summarize_thread_args(project_key, thread_id, llm_mode),
        )
    }

    fn summarize_thread_product(
        &self,
        product_key: &str,
        thread_id: &str,
        llm_mode: bool,
    ) -> Result<Value> {
        self.call(
            "summarize_thread_product",
            wire::summarize_thread_product_args(product_key, thread_id, llm_mode),
        )
    }

    fn request_contact(
        &self,
        project_key: &str,
        from_agent: &str,
        to_agent: &str,
        to_project: Option<&str>,
        reason: &str,
    ) -> Result<Value> {
        self.call(
            "request_contact",
            wire::request_contact_args(project_key, from_agent, to_agent, to_project, reason),
        )
    }

    fn respond_contact(
        &self,
        project_key: &str,
        to_agent: &str,
        from_agent: &str,
        accept: bool,
    ) -> Result<Value> {
        self.call(
            "respond_contact",
            wire::respond_contact_args(project_key, to_agent, from_agent, accept),
        )
    }

    fn list_contacts(&self, project_key: &str, agent: &str) -> Result<Value> {
        self.call(
            "list_contacts",
            wire::list_contacts_args(project_key, agent),
        )
    }

    fn set_contact_policy(&self, project_key: &str, agent: &str, policy: &str) -> Result<Value> {
        self.call(
            "set_contact_policy",
            wire::set_contact_policy_args(project_key, agent, policy),
        )
    }

    fn ensure_product(&self, product_key: &str) -> Result<Value> {
        self.call("ensure_product", wire::ensure_product_args(product_key))
    }

    fn products_link(&self, product_key: &str, project_key: &str) -> Result<Value> {
        self.call(
            "products_link",
            wire::products_link_args(product_key, project_key),
        )
    }

    fn search_product(&self, product_key: &str, query: &str, limit: u32) -> Result<Value> {
        self.call(
            "search_messages_product",
            wire::search_product_args(product_key, query, limit),
        )
    }

    fn inbox_product(&self, product_key: &str, agent: &str, limit: u32) -> Result<Value> {
        self.call(
            "fetch_inbox_product",
            wire::inbox_product_args(product_key, agent, limit),
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
        let b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into()).unwrap();
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
        let b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into()).unwrap();
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

    /// se2.2: a non-decimal message_id must Err naming the decimal format —
    /// never silently become 0 (toron hex ids belong to the toron backend,
    /// not this one). The parse runs before any dialing, so a dead server is
    /// indistinguishable from a parse failure: the error must say "decimal".
    #[test]
    fn ack_rejects_non_decimal_message_id() {
        let b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into()).unwrap();
        match b.ack("proj", "Orchid", "deadbeef") {
            Err(BackendError::Other(e)) => {
                let s = format!("{e:#}");
                assert!(s.contains("decimal"), "names the id format: {s}");
                assert!(
                    !s.contains("not reachable"),
                    "parse fails before any dial: {s}"
                );
            }
            Err(e) => panic!("expected Other, got {e:?}"),
            Ok(_) => panic!("non-decimal id cannot ack"),
        }
    }

    /// se2.2: same honesty for mark_read — a hex string must Err, never 0.
    #[test]
    fn mark_read_rejects_non_decimal_message_id() {
        let b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into()).unwrap();
        match b.mark_read("proj", "Orchid", "a1b2c3d4") {
            Err(BackendError::Other(e)) => {
                assert!(
                    format!("{e:#}").contains("decimal"),
                    "names the id format: {e:#}"
                );
            }
            Err(e) => panic!("expected Other, got {e:?}"),
            Ok(_) => panic!("non-decimal id cannot mark_read"),
        }
    }

    /// Pane-identity write is a LOCAL filesystem op (no dialing): it succeeds
    /// even against a dead server, writing the canonical identity file. The
    /// identity base is pinned to a temp dir (test hook) so the test never
    /// touches the real `~/.config` and is immune to parallel env mutation.
    #[test]
    fn write_pane_identity_is_local_not_network() {
        let base = std::env::temp_dir().join(format!("fw-am-idbase-{}", std::process::id()));
        crate::mail_shim::set_test_identity_base(Some(base));
        let b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into()).unwrap();
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
        crate::mail_shim::set_test_identity_base(None);
    }
}
