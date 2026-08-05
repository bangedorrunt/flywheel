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
use serde_json::Value;

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

    /// thread_id is accepted by the trait but the current `Mail::send` has no
    /// thread param — ignored until bd-mail-reply-thread adds it.
    fn send(
        &mut self,
        project_key: &str,
        to: &[String],
        subject: &str,
        body: &str,
        importance: &str,
        ack_required: bool,
        _thread_id: Option<&str>,
    ) -> Result<Value> {
        self.mail
            .send(project_key, to, subject, body, importance, ack_required)
            .map_err(BackendError::Other)
    }

    fn reply(
        &mut self,
        _project_key: &str,
        _message_id: u64,
        _body: &str,
        _to: Option<&[String]>,
    ) -> Result<Value> {
        // Phase 3: bd-mail-reply-thread.
        Err(self.unsupported("reply"))
    }

    fn inbox(&mut self, project_key: &str, agent: &str, filter: &InboxFilter) -> Result<Value> {
        // Mail::inbox only exposes urgent_only + limit; the richer filter set
        // (unread_only/ack_overdue_only/since_ts/include_bodies) lands with
        // bd-mail-ack-sla-tracking which extends Mail::inbox. Until then the
        // trait forwards what Mail supports.
        self.mail
            .inbox(project_key, agent, filter.urgent_only, filter.limit)
            .map_err(BackendError::Other)
    }

    fn ack(&mut self, _project_key: &str, _agent: &str, _message_id: u64) -> Result<Value> {
        // Phase 3: bd-mail-ack-sla-tracking.
        Err(self.unsupported("ack"))
    }

    fn mark_read(&mut self, _project_key: &str, _agent: &str, _message_id: u64) -> Result<Value> {
        // Phase 3: bd-mail-ack-sla-tracking.
        Err(self.unsupported("mark_read"))
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

    fn slot_acquire(
        &mut self,
        _project_key: &str,
        _agent: &str,
        _slot: &str,
        _exclusive: bool,
        _ttl_seconds: u64,
    ) -> Result<Value> {
        // Phase 3: bd-mail-build-slot-gating.
        Err(self.unsupported("slot_acquire"))
    }

    fn slot_release(&mut self, _project_key: &str, _agent: &str, _slot: &str) -> Result<Value> {
        // Phase 3: bd-mail-build-slot-gating.
        Err(self.unsupported("slot_release"))
    }

    fn search(
        &mut self,
        _project_key: &str,
        _query: &str,
        _limit: u32,
        _filter: &crate::backend::SearchFilter,
    ) -> Result<Value> {
        // Phase 3: bd-mail-search-summarize.
        Err(self.unsupported("search"))
    }

    fn summarize_thread(
        &mut self,
        _project_key: &str,
        _thread_id: &str,
        _llm_mode: bool,
    ) -> Result<Value> {
        // Phase 3: bd-mail-reply-thread (also owns the Mail::summarize wrapper).
        Err(self.unsupported("summarize_thread"))
    }

    fn request_contact(
        &mut self,
        _project_key: &str,
        _from_agent: &str,
        _to_agent: &str,
        _to_project: Option<&str>,
        _reason: &str,
    ) -> Result<Value> {
        // Phase 3: bd-mail-cross-project-contact.
        Err(self.unsupported("request_contact"))
    }

    fn respond_contact(
        &mut self,
        _project_key: &str,
        _to_agent: &str,
        _from_agent: &str,
        _accept: bool,
    ) -> Result<Value> {
        // Phase 3: bd-mail-cross-project-contact.
        Err(self.unsupported("respond_contact"))
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
        // register_agent USED to be Unsupported; bd-mail-register-on-spawn
        // graduated it to real delegation (calls Mail::register_agent).
        // renew graduated with bd-mail-lease-renewal-durable. The remaining 9
        // stay Unsupported until their Phase-3 beads land.
        let cases: Vec<(&str, std::result::Result<Value, BackendError>)> = vec![
            ("reply", b.reply("p", 1, "x", None)),
            ("ack", b.ack("p", "a", 1)),
            ("mark_read", b.mark_read("p", "a", 1)),
            (
                "slot_acquire",
                b.slot_acquire("p", "a", "cargo", true, 1800),
            ),
            ("slot_release", b.slot_release("p", "a", "cargo")),
            ("search", b.search("p", "q", 5, &Default::default())),
            ("summarize_thread", b.summarize_thread("p", "t", false)),
            (
                "request_contact",
                b.request_contact("p", "a", "b", None, "r"),
            ),
            ("respond_contact", b.respond_contact("p", "b", "a", true)),
        ];
        for (method, res) in cases {
            match res {
                Err(BackendError::Unsupported {
                    backend, method: m, ..
                }) => {
                    assert_eq!(backend, "agentmail", "backend tag for {method}");
                    assert_eq!(m, method, "method tag matches");
                }
                other => panic!("{method}: expected Unsupported, got {other:?}"),
            }
        }
    }
}
