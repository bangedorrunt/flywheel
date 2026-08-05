//! agent-mail-MCP backend — wraps `crate::mail::Mail` (the HTTP JSON-RPC
//! client) as the default `MailBackend` impl. Pure delegation for the 8
//! operations `Mail` already wraps; `BackendError::Unsupported` for the ~10
//! trait methods whose MCP-tool wrappers land in ADR-0001 Phase 3 (each
//! Phase-3 bead graduates a method from `Unsupported` to real delegation +
//! behavior).
//!
//! governed-by: ADR-0002  ·  implements: `MailBackend`
//!
//! Design: `Mail` carries `name` as a field (the sending agent's identity).
//! `MailBackend::reserve`/`release`/`conflicts` take an explicit `agent`
//! param so the future `ToronMailBackend` can target arbitrary agents; this
//! impl forwards `self.mail.name.clone()` (the orchestrator always acts as
//! itself). The trait's `send` carries `thread_id`; the current `Mail::send`
//! doesn't pass it (no MCP-thread wrapper exists yet — Phase 3), so `thread_id`
//! is ignored here and honored once `bd-mail-reply-thread` lands.

use crate::backend::{BackendError, InboxFilter, MailBackend, Result};
use crate::mail::Mail;
use serde_json::{json, Value};

/// Default mail backend: delegates to the agent-mail MCP HTTP client.
pub struct AgentMailMcpBackend {
    mail: Mail,
}

impl AgentMailMcpBackend {
    /// Construct from the existing `Mail` client (which `Mail::from_config`
    /// builds from `MailConfig`). The factory (`bd-backend-env-config`) calls
    /// this; tests can inject a `Mail` directly.
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
        // full filter set lands with bd-mail-ack-sla-tracking (Mail::inbox_filtered)
        let mail_filter = crate::mail::MailInboxFilter {
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
        // The MCP tool reads the identity files the bridge writes
        // (bd-mail-pane-identity-bridge). A missing identity surfaces as a
        // non-JSON isError text — callers treat that as not-found.
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
        // SearchFilter struct to the wrapper's optional params.
        self.mail
            .search(
                project_key,
                query,
                limit,
                filter.sender.as_deref(),
                filter.importance.as_deref(),
                filter.thread_id.as_deref(),
                filter.date_start.as_deref(),
                filter.date_end.as_deref(),
                filter.ranking.as_deref(),
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
    /// client can't be constructed without a config + URL, so this is a
    /// compile-time object-safety check rather than a runtime round-trip;
    /// HTTP round-trips are integration-test territory (R0.8 parity test).
    #[test]
    fn agentmail_backend_is_object_safe() {
        fn _assert_dyn(_: &dyn MailBackend) {}
        // We can't construct AgentMailMcpBackend without a Mail here; the type
        // system proves the impl satisfies the trait by compilation alone.
        // (R0.8 bd-backend-parity-test exercises the real HTTP path.)
    }

    /// name() reports the right identifier — the one piece we can assert
    /// without a live Mail. Constructed via a tiny helper that bypasses
    /// from_config (the struct field is private; the test lives in-module).
    #[test]
    fn agentmail_name_is_agentmail() {
        // Build a Mail with a placeholder url so the struct exists; no HTTP
        // is made until a method is called.
        let mail = Mail::from_config(Some(&crate::config::MailConfig {
            url: Some("http://127.0.0.1:9999".into()),
            token: None,
            name: Some("test-orchestrator".into()),
        }))
        .expect("from_config with a url yields Some(Mail)")
        .expect("Some(Mail) unwraps");
        let b = AgentMailMcpBackend::new(mail);
        assert_eq!(b.name(), "agentmail");
    }

    /// `renew` delegates to `Mail::renew` (bd-mail-lease-renewal-durable) —
    /// compile-time signature guard; no HTTP is made (dummy URL).
    #[test]
    fn renew_is_real_delegation_not_unsupported() {
        let mail = Mail::from_config(Some(&crate::config::MailConfig {
            url: Some("http://127.0.0.1:9999".into()),
            token: None,
            name: Some("test-orchestrator".into()),
        }))
        .expect("from_config ok")
        .expect("Some");
        let mut b = AgentMailMcpBackend::new(mail);
        // an Unsupported error would mean the Phase-3 stub is still in place;
        // a connection failure means it genuinely delegated to Mail::renew
        match b.renew("p", "a", 1200, None) {
            Err(BackendError::Other(_)) => {}
            Err(e) => panic!("renew should delegate (Other), got {e:?}"),
            Ok(_) => panic!("renew cannot succeed against a dummy URL"),
        }
    }

    /// `resolve_pane_identity` delegates to the MCP tool (bd-mail-pane-
    /// identity-bridge) — compile-time + runtime shape guard; no HTTP is
    /// made successfully (dummy URL), so `Other` proves real delegation.
    #[test]
    fn resolve_pane_identity_is_real_delegation_not_unsupported() {
        let mail = Mail::from_config(Some(&crate::config::MailConfig {
            url: Some("http://127.0.0.1:9999".into()),
            token: None,
            name: Some("test-orchestrator".into()),
        }))
        .expect("from_config ok")
        .expect("Some");
        let mut b = AgentMailMcpBackend::new(mail);
        match b.resolve_pane_identity("p", "w1:p1") {
            Err(BackendError::Other(_)) => {}
            Err(e) => panic!("resolve_pane_identity should delegate (Other), got {e:?}"),
            Ok(_) => panic!("resolve cannot succeed against a dummy URL"),
        }
    }

    /// An Unsupported method returns the right BackendError shape (no panic,
    /// no silent Ok). Covers the Phase-3 stubs in one parameterized check.
    #[test]
    fn unsupported_methods_report_unsupported_not_panic() {
        let mail = Mail::from_config(Some(&crate::config::MailConfig {
            url: Some("http://127.0.0.1:9999".into()),
            token: None,
            name: Some("test-orchestrator".into()),
        }))
        .expect("from_config ok")
        .expect("Some");
        let mut b = AgentMailMcpBackend::new(mail);
        // All prior Phase-3 stubs have graduated (including
        // bd-mail-cross-project-contact: request_contact/respond_contact).
        // Dummy URL → BackendError::Other (delegation), never Unsupported.
        for (method, res) in [
            (
                "request_contact",
                b.request_contact("p", "a", "b", None, "r"),
            ),
            ("respond_contact", b.respond_contact("p", "b", "a", true)),
            ("list_contacts", b.list_contacts("p", "a")),
            ("set_contact_policy", b.set_contact_policy("p", "a", "auto")),
            ("ensure_product", b.ensure_product("trading-research")),
            ("products_link", b.products_link("trading-research", "p")),
        ] {
            match res {
                Err(BackendError::Other(_)) => {}
                Err(BackendError::Unsupported { method: m, .. }) => {
                    panic!("{method}: still Unsupported ({m}) — should delegate")
                }
                Ok(_) => panic!("{method}: cannot succeed against dummy URL"),
                Err(e) => panic!("{method}: unexpected {e:?}"),
            }
        }
    }
}
