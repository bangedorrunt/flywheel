//! agent-mail-MCP backend — wraps `crate::mail_shim::Mail` (the IN-PROCESS
//! toron-core mailbox facade, ADR-0003 Decision 3A) as the default
//! `MailBackend` impl. Pure delegation for the operations `Mail` already
//! wraps; `BackendError::Unsupported` for trait methods without a delegation.
//!
//! governed-by: ADR-0002, ADR-0003  ·  implements: `MailBackend`
//!
//! Design: `Mail` carries `name` as a field (the sending agent's identity).
//! `MailBackend::reserve`/`release`/`conflicts` take an explicit `agent`
//! param so a future backend can target arbitrary agents; this impl forwards
//! `self.mail.name.clone()` (the orchestrator always acts as itself).

use crate::backend::{BackendError, InboxFilter, MailBackend, Result};
use crate::mail_shim::Mail;
use serde_json::{json, Value};

/// Default mail backend: delegates to the agent-mail MCP HTTP client.
pub struct AgentMailMcpBackend {
    mail: Mail,
}

impl AgentMailMcpBackend {
    /// Construct from the in-process `Mail` facade (which `Mail::in_process`
    /// builds; `[agent_mail]` config is optional). The factory
    /// (`bd-backend-env-config`) calls this; tests can inject a `Mail` directly.
    pub fn new(mail: Mail) -> Self {
        Self { mail }
    }

    fn unsupported(&self, method: &'static str) -> BackendError {
        BackendError::Unsupported {
            backend: "agentmail".to_string(),
            method,
            reason:
                "not yet implemented on the agentmail backend; see the owning ADR-0001 Phase-3 bead"
                    .to_string(),
        }
    }
}

impl MailBackend for AgentMailMcpBackend {
    fn name(&self) -> &'static str {
        "agentmail"
    }

    fn agent_name(&self) -> &str {
        &self.mail.name
    }

    fn health(&mut self) -> Result<Value> {
        self.mail.health().map_err(BackendError::Other)
    }

    fn ensure_project(&mut self, project_key: &str) -> Result<Value> {
        self.mail
            .ensure_project(project_key)
            .map_err(BackendError::Other)
    }

    fn register_agent(
        &mut self,
        project_key: &str,
        name: &str,
        program: &str,
        model: &str,
        task: &str,
    ) -> Result<Value> {
        self.mail
            .register_agent(project_key, name, program, model, task)
            .map_err(BackendError::Other)
    }

    fn list_agents(&mut self, project_key: &str) -> Result<Value> {
        self.mail
            .list_agents(project_key)
            .map_err(BackendError::Other)
    }

    /// thread_id chains into a conversation thread (bd-mail-reply-thread).
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
        self.mail
            .send(
                project_key,
                to,
                subject,
                body,
                importance,
                ack_required,
                thread_id,
            )
            .map_err(BackendError::Other)
    }

    fn reply(
        &mut self,
        project_key: &str,
        message_id: u64,
        body: &str,
        to: Option<&[String]>,
    ) -> Result<Value> {
        self.mail
            .reply(project_key, message_id, body, to)
            .map_err(BackendError::Other)
    }

    fn inbox(&mut self, project_key: &str, agent: &str, filter: &InboxFilter) -> Result<Value> {
        let mail_filter = crate::mail_shim::MailInboxFilter {
            urgent_only: filter.urgent_only,
            unread_only: filter.unread_only,
            ack_overdue_only: filter.ack_overdue_only,
            include_bodies: filter.include_bodies,
            limit: filter.limit,
            since_ts: filter.since_ts.clone(),
        };
        self.mail
            .inbox_filtered(project_key, agent, &mail_filter)
            .map_err(BackendError::Other)
    }

    fn ack(&mut self, project_key: &str, agent: &str, message_id: u64) -> Result<Value> {
        self.mail
            .ack(project_key, agent, message_id)
            .map_err(BackendError::Other)
    }

    fn mark_read(&mut self, project_key: &str, agent: &str, message_id: u64) -> Result<Value> {
        self.mail
            .mark_read(project_key, agent, message_id)
            .map_err(BackendError::Other)
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
        // Mail carries the agent name as a field; the explicit `agent` param
        // is for the toron backend's per-agent targeting. Here we act as self.
        let _ = _agent; // explicit-ignore: documented above
        self.mail
            .reserve(project_key, paths, ttl_seconds, exclusive, reason)
            .map_err(BackendError::Other)
    }

    fn release(&mut self, project_key: &str, _agent: &str) -> Result<Value> {
        let _ = _agent;
        self.mail.release(project_key).map_err(BackendError::Other)
    }

    fn renew(
        &mut self,
        project_key: &str,
        _agent: &str,
        extend_seconds: u64,
        paths: Option<&[String]>,
    ) -> Result<Value> {
        // Mail carries the agent name as a field; the explicit `agent` param
        // is for the toron backend's per-agent targeting. Here we act as self.
        // IDs are not exposed through the trait surface — `None` renews all of
        // this agent's active reservations (bd-mail-lease-renewal-durable).
        let _ = _agent; // explicit-ignore: documented above
        let agent = self.mail.name.clone();
        self.mail
            .renew(project_key, &agent, extend_seconds, paths, None)
            .map_err(BackendError::Other)
    }

    fn conflicts(&mut self, project_key: &str, _agent: &str, paths: &[String]) -> Result<Value> {
        let _ = _agent;
        self.mail
            .conflicts(project_key, paths)
            .map_err(BackendError::Other)
    }

    fn resolve_pane_identity(&mut self, project_key: &str, pane_id: &str) -> Result<Value> {
        // In-process read of the canonical identity file the bridge writes
        // (bd-mail-pane-identity-bridge). A missing identity surfaces as JSON
        // without `agent_name` — callers treat that as not-found.
        self.mail
            .resolve_pane_identity(project_key, pane_id)
            .map_err(BackendError::Other)
    }

    fn write_pane_identity(
        &mut self,
        project_key: &str,
        pane_id: &str,
        agent_name: &str,
    ) -> Result<Value> {
        let path = self
            .mail
            .write_pane_identity(project_key, pane_id, agent_name)
            .map_err(BackendError::Other)?;
        Ok(json!({ "path": path.to_string_lossy() }))
    }

    fn clear_pane_identity(&mut self, project_key: &str, pane_id: &str) -> Result<Value> {
        let path = self
            .mail
            .clear_pane_identity(project_key, pane_id)
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
        // bd-mail-build-slot-gating: delegate to Mail::acquire_slot. The slot
        // is advisory (returns conflicts, doesn't hard-block); the caller
        // decides to wait. TTL is the safety net against a forgotten release.
        self.mail
            .acquire_slot(project_key, agent, slot, exclusive, ttl_seconds)
            .map_err(BackendError::Other)
    }

    fn slot_release(&mut self, project_key: &str, agent: &str, slot: &str) -> Result<Value> {
        // bd-mail-build-slot-gating: delegate to Mail::release_slot. Must be
        // called after a build; the TTL auto-expires if forgotten.
        self.mail
            .release_slot(project_key, agent, slot)
            .map_err(BackendError::Other)
    }

    fn search(
        &mut self,
        project_key: &str,
        query: &str,
        limit: u32,
        filter: &crate::backend::SearchFilter,
    ) -> Result<Value> {
        // bd-mail-search-summarize: delegate to Mail::search, mapping the
        // trait SearchFilter to the wrapper's filter struct (review-fix F8).
        self.mail
            .search(
                project_key,
                query,
                limit,
                &crate::mail_shim::MailSearchFilter {
                    sender: filter.sender.clone(),
                    importance: filter.importance.clone(),
                    thread_id: filter.thread_id.clone(),
                    date_start: filter.date_start.clone(),
                    date_end: filter.date_end.clone(),
                    ranking: filter.ranking.clone(),
                    include_body_md: filter.include_body_md,
                },
            )
            .map_err(BackendError::Other)
    }

    fn summarize_thread(
        &mut self,
        project_key: &str,
        thread_id: &str,
        llm_mode: bool,
    ) -> Result<Value> {
        self.mail
            .summarize_thread(project_key, thread_id, llm_mode)
            .map_err(BackendError::Other)
    }

    fn request_contact(
        &mut self,
        project_key: &str,
        from_agent: &str,
        to_agent: &str,
        to_project: Option<&str>,
        reason: &str,
    ) -> Result<Value> {
        self.mail
            .request_contact(project_key, from_agent, to_agent, to_project, reason, None)
            .map_err(BackendError::Other)
    }

    fn respond_contact(
        &mut self,
        project_key: &str,
        to_agent: &str,
        from_agent: &str,
        accept: bool,
    ) -> Result<Value> {
        self.mail
            .respond_contact(project_key, to_agent, from_agent, accept)
            .map_err(BackendError::Other)
    }

    fn list_contacts(&mut self, project_key: &str, agent: &str) -> Result<Value> {
        self.mail
            .list_contacts(project_key, agent)
            .map_err(BackendError::Other)
    }

    fn set_contact_policy(
        &mut self,
        project_key: &str,
        agent: &str,
        policy: &str,
    ) -> Result<Value> {
        self.mail
            .set_contact_policy(project_key, agent, policy)
            .map_err(BackendError::Other)
    }

    fn ensure_product(&mut self, product_key: &str) -> Result<Value> {
        self.mail
            .ensure_product(product_key)
            .map_err(BackendError::Other)
    }

    fn products_link(&mut self, product_key: &str, project_key: &str) -> Result<Value> {
        self.mail
            .products_link(product_key, project_key)
            .map_err(BackendError::Other)
    }

    fn search_product(&mut self, product_key: &str, query: &str, limit: u32) -> Result<Value> {
        self.mail
            .search_product(product_key, query, limit)
            .map_err(BackendError::Other)
    }

    fn inbox_product(&mut self, product_key: &str, agent: &str, limit: u32) -> Result<Value> {
        self.mail
            .inbox_product(product_key, agent, limit)
            .map_err(BackendError::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MailBackend;

    /// The struct exists and can be named as `dyn MailBackend`. The `Mail`
    /// facade is constructed in-process (no config, no HTTP); the type system
    /// proves the impl satisfies the trait by compilation alone.
    #[test]
    fn agentmail_backend_is_object_safe() {
        fn _assert_dyn(_: &dyn MailBackend) {}
        // Can't construct AgentMailMcpBackend without a Mail here; the type
        // system proves the impl satisfies the trait by compilation alone.
    }

    /// name() reports the right identifier — the one piece we can assert
    /// without a live Mail. Constructed via `Mail::in_process` (in-process).
    #[test]
    fn agentmail_name_is_agentmail() {
        let mail = Mail::in_process(Some(&crate::config::MailConfig {
            url: Some("http://127.0.0.1:9999".into()),
            token: None,
            name: Some("test-orchestrator".into()),
        }))
        .expect("in_process yields Some(Mail)")
        .expect("Some(Mail) unwraps");
        let b = AgentMailMcpBackend::new(mail);
        assert_eq!(b.name(), "agentmail");
    }

    /// `renew` delegates to `Mail::renew` (bd-mail-lease-renewal-durable) —
    /// against the unregistered agent the in-process mailbox surfaces an
    /// `Other` error, proving real delegation rather than `Unsupported`.
    #[test]
    fn renew_is_real_delegation_not_unsupported() {
        let mail = Mail::in_process(None).expect("in_process ok").expect("Some");
        let mut b = AgentMailMcpBackend::new(mail);
        match b.renew("p", "a", 1200, None) {
            Err(BackendError::Other(_)) => {}
            Err(e) => panic!("renew should delegate (Other), got {e:?}"),
            Ok(_) => panic!("renew cannot succeed for an unregistered agent"),
        }
    }

    /// `resolve_pane_identity` reads the canonical identity file in-process —
    /// a missing file is a not-found JSON (Ok), NOT `Unsupported` (which
    /// would mean the pane-identity bridge is still stubbed).
    #[test]
    fn resolve_pane_identity_is_real_delegation_not_unsupported() {
        let mail = Mail::in_process(None).expect("in_process ok").expect("Some");
        let mut b = AgentMailMcpBackend::new(mail);
        match b.resolve_pane_identity("p", "w1:p1") {
            Ok(v) => assert!(v.get("agent_name").is_none(), "no identity file"),
            Err(e) => panic!("resolve should be Ok(not-found), got {e:?}"),
        }
    }

    /// No method returns `Unsupported` — every op genuinely delegates to the
    /// in-process mailbox. Ops that need a registered agent error `Other`
    /// (delegation to the unknown-name path); operator-backed product ops
    /// succeed. A real `Unsupported` would mean the delegation is still stubbed.
    #[test]
    fn unsupported_methods_report_unsupported_not_panic() {
        let mail = Mail::in_process(None).expect("in_process ok").expect("Some");
        let mut b = AgentMailMcpBackend::new(mail);
        // (method, result, expected-ok) — never Unsupported, never panic.
        for (method, res, expected_ok) in [
            (
                "request_contact",
                b.request_contact("p", "a", "b", None, "r"),
                false, // from-agent not registered
            ),
            ("respond_contact", b.respond_contact("p", "b", "a", true), false),
            ("list_contacts", b.list_contacts("p", "a"), false),
            ("set_contact_policy", b.set_contact_policy("p", "a", "auto"), false),
            ("ensure_product", b.ensure_product("trading-research"), true),
            ("products_link", b.products_link("trading-research", "p"), true),
        ] {
            match res {
                Ok(_) if expected_ok => {}
                Ok(_) => panic!("{method}: unexpected Ok for an unregistered agent"),
                Err(BackendError::Other(_)) if !expected_ok => {}
                Err(BackendError::Other(_)) => panic!("{method}: should succeed with an operator"),
                Err(BackendError::Unsupported { method: m, .. }) => {
                    panic!("{method}: still Unsupported ({m}) — should delegate")
                }
            }
        }
    }
}
