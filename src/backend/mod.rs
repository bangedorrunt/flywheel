//! Pluggable backend abstraction — trait surface for the multiplexer (herdr /
//! superlogical / tmux) and the mail fabric (agent-mail MCP / toron).
//!
//! governed-by: ADR-0002
//!
//! Phase 0 (R0): define the traits. Current concrete clients (`herdr::Client`,
//! `mail::Mail`) are wrapped by their backend impls in sibling beads
//! (`bd-herdr-backend-impl`, `bd-agentmail-backend-impl`). The factory
//! (`bd-backend-env-config`) selects the active backend from
//! `FLYWHEEL_BACKEND` / `FLYWHEEL_MAIL`. Trait signatures mirror toron ADR-0003
//! §architecture layer 5 so the R1.5 merge (toron-core owns the trait) is
//! mechanical.
//!
//! Design: every method except `name()` has a default body returning
//! `BackendError::Unsupported`. Real impls override what they support; future
//! backends inherit `Unsupported` for operations they don't yet implement
//! (ADR-0002 §6 — open-to-extension, not sealed). The trait is object-safe
//! (`Box<dyn MultiplexerBackend>` / `Box<dyn MailBackend>`).
//!
//! Phase-0 caveat: this whole module is the trait surface defined ahead of
//! its consumers. Nothing constructs/uses it outside `#[cfg(test)]` until
//! `bd-herdr-backend-impl` / `bd-agentmail-backend-impl` / `bd-backend-env-config`
//! land, so `dead_code` is allowed module-wide here and removed when those
//! beads wire the factory + first real impl.

// TODO(bd-herdr-backend-impl): drop this allow once a real impl consumes the surface.
#![allow(dead_code)]

pub mod agentmail;
pub mod factory;
pub mod herdr;
#[cfg(test)]
mod parity;
#[cfg(feature = "backend-superlogical")]
pub mod superlogical;
#[cfg(feature = "backend-tmux")]
pub mod tmux;
#[cfg(feature = "mail-toron")]
pub mod toron_mail;

use crate::herdr::{
    AgentInfo, AgentStatus, PaneInfo, PushEvent, Subscription, WaitOptions, WorkspaceInfo,
};
use serde_json::Value;

/// Landing data for a coded `agent.prompt` (ADR-0002, review-fix F6).
///
/// Mirrors `herdr::classify_send_result` inputs so a backend-agnostic `__send`
/// leaf can make the same landed/stalled/timeout/coded dispatch decisions as
/// the concrete herdr client:
/// - `status: Some(s)` → the prompt landed and the agent reached `s`.
/// - `code: Some(c)` → herdr returned a protocol error code
///   (e.g. `agent_prompt_stalled`, `timeout`, `agent_not_running`).
///
/// Exactly one of the two is populated on the herdr backend (landed → status,
/// error → code); other backends define their own contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptLanding {
    pub status: Option<AgentStatus>,
    pub code: Option<String>,
}

// Factory entry points — consumers (the durable-loop leaf handlers in main.rs)
// call `backend::multiplexer_backend(&cfg)` / `backend::mail_backend(&cfg)`;
// `flywheel backend list/status` (bd-cli-backend-subcommand) uses the resolve_
// and compiled_backends helpers.
pub use factory::{
    compiled_backends, mail_backend, multiplexer_backend, resolve_backend_name, resolve_mail_name,
};

/// Filter bag for `MailBackend::inbox` (fields only; semantics land with the
/// beads that consume them — `bd-mail-ack-sla-tracking` owns the ack-overdue
/// meaning, etc.).
#[derive(Debug, Clone, Default)]
pub struct InboxFilter {
    pub urgent_only: bool,
    pub unread_only: bool,
    pub ack_overdue_only: bool,
    pub include_bodies: bool,
    pub limit: u32,
    pub since_ts: Option<String>,
}

/// Filter bag for `MailBackend::search`.
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub sender: Option<String>,
    pub importance: Option<String>,
    pub thread_id: Option<String>,
    pub date_start: Option<String>,
    pub date_end: Option<String>,
    pub ranking: Option<String>,
    /// Include the full `body_md` field on each result. agent-mail's
    /// `search_messages` returns subject-only unless this is set; the
    /// collaboration patterns (debate, plan-review) harvest reply bodies via
    /// `harvest_latest`, so this flag must be expressible at the TRAIT level
    /// (the `MailBackend::search` method takes `&SearchFilter`).
    ///
    /// governed-by: ADR-0001 §9.1/§9.2 (review-fix F1, bd-review-fix-epic-m0b.1)
    pub include_body_md: bool,
}

/// Errors a backend may surface. `Unsupported` is the honest signal for a
/// method a backend doesn't implement (real impl gap, or a stub backend like
/// the unreleased `superlogical`/pre-R1 `toron`). `Other` wraps impl-specific
/// errors (anyhow) so real impls can use `?` via the `From` impl.
#[derive(Debug)]
pub enum BackendError {
    Unsupported {
        backend: String,
        method: &'static str,
        reason: String,
    },
    Other(anyhow::Error),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Unsupported {
                backend,
                method,
                reason,
            } => write!(
                f,
                "backend '{backend}' does not implement '{method}': {reason}"
            ),
            BackendError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<anyhow::Error> for BackendError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

impl From<crate::herdr::HerdrError> for BackendError {
    /// Preserve herdr's typed error code as an `Other` with a lossless
    /// anyhow context. (Backend callers handle `BackendError::Unsupported`;
    /// transport errors stay opaque in `Other`.)
    fn from(e: crate::herdr::HerdrError) -> Self {
        Self::Other(anyhow::anyhow!("herdr error {}: {}", e.code, e.message))
    }
}

impl BackendError {
    /// Helper for default trait method bodies: the named op is unsupported by
    /// the named backend.
    fn unsupported(backend: &str, method: &'static str) -> Self {
        Self::Unsupported {
            backend: backend.to_string(),
            method,
            reason: format!("backend '{backend}' does not implement '{method}'"),
        }
    }

    /// Helper for stub backends: a whole-backend reason (not per-method). Used
    /// by the superlogical/tmux/toron stubs (bd-toron-superlogical-stubs).
    pub(crate) fn stub(backend: &'static str, reason: &str) -> Self {
        Self::Unsupported {
            backend: backend.to_string(),
            method: "(stub)",
            reason: reason.to_string(),
        }
    }
}

/// Type alias so trait signatures read `Result<T>` and real impls get
/// `anyhow::Error -> BackendError` coercion for free via the `From` impl.
pub type Result<T> = std::result::Result<T, BackendError>;

/// Pushed-event stream returned by `MultiplexerBackend::subscribe`. herdr's
/// concrete `EventStream` is wrapped by `HerdrBackend` (R0.2) to satisfy this
/// trait; future backends provide their own impl. `read_event` is the only
/// required method; `set_read_timeout` defaults to a no-op.
pub trait EventStream: Send {
    fn read_event(&mut self) -> Result<Option<PushEvent>>;
    fn set_read_timeout(&mut self, _timeout: Option<std::time::Duration>) -> Result<()> {
        Ok(())
    }
}

/// Terminal / workspace / pane / agent-lifecycle backend. Default = herdr;
/// future = superlogical; legacy = tmux. Method set mirrors toron ADR-0003
/// §architecture layer 5. All methods take `&mut self` because the underlying
/// socket/HTTP clients are stateful (per-call request ids).
pub trait MultiplexerBackend: Send + Sync {
    /// Stable backend identifier ("herdr" | "superlogical" | "tmux").
    fn name(&self) -> &'static str;

    fn workspace_list(&mut self) -> Result<Vec<WorkspaceInfo>> {
        Err(BackendError::unsupported(self.name(), "workspace_list"))
    }
    fn workspace_create(&mut self, _label: Option<&str>) -> Result<WorkspaceInfo> {
        Err(BackendError::unsupported(self.name(), "workspace_create"))
    }
    fn workspace_close(&mut self, _workspace_id: &str) -> Result<()> {
        Err(BackendError::unsupported(self.name(), "workspace_close"))
    }
    fn pane_split(
        &mut self,
        _parent_pane_id: &str,
        _direction: &str,
        _cwd: Option<&str>,
    ) -> Result<PaneInfo> {
        Err(BackendError::unsupported(self.name(), "pane_split"))
    }
    fn pane_list(&mut self, _workspace_id: Option<&str>) -> Result<Vec<PaneInfo>> {
        Err(BackendError::unsupported(self.name(), "pane_list"))
    }
    fn pane_close(&mut self, _pane_id: &str) -> Result<()> {
        Err(BackendError::unsupported(self.name(), "pane_close"))
    }
    fn pane_run(&mut self, _pane_id: &str, _command: &str) -> Result<()> {
        Err(BackendError::unsupported(self.name(), "pane_run"))
    }
    fn pane_wait_output(
        &mut self,
        _pane_id: &str,
        _needle: &str,
        _timeout_ms: u64,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "pane_wait_output"))
    }
    fn agent_list(&mut self) -> Result<Vec<AgentInfo>> {
        Err(BackendError::unsupported(self.name(), "agent_list"))
    }
    fn agent_start(
        &mut self,
        _name: &str,
        _kind: &str,
        _pane_id: &str,
        _args: &[String],
    ) -> Result<AgentInfo> {
        Err(BackendError::unsupported(self.name(), "agent_start"))
    }
    fn agent_rename(&mut self, _target: &str, _new_name: Option<&str>) -> Result<()> {
        Err(BackendError::unsupported(self.name(), "agent_rename"))
    }
    fn agent_prompt(
        &mut self,
        _target: &str,
        _text: &str,
        _wait: Option<&WaitOptions>,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "agent_prompt"))
    }
    /// Coded `agent.prompt` with landing semantics (ADR-0002, review-fix F6).
    ///
    /// The plain [`Self::agent_prompt`] returns a minimal JSON and LOSES the
    /// coded error (e.g. `agent_prompt_stalled`) that the durable loop's
    /// `__send` leaf classifies into landed/stalled/timeout. This method
    /// returns the landing data (observed status + protocol error code)
    /// without erroring on a non-landing outcome, so a backend-agnostic
    /// `__send` can make the same dispatch decisions as the concrete herdr
    /// client.
    ///
    /// `wait` semantics mirror herdr: when Some, block until the agent
    /// reaches `until` or the timeout; landing = the prompt started
    /// executing (working/blocked), NOT idle/done.
    fn agent_prompt_coded(
        &mut self,
        _target: &str,
        _text: &str,
        _wait: Option<&WaitOptions>,
    ) -> Result<PromptLanding> {
        Err(BackendError::unsupported(self.name(), "agent_prompt_coded"))
    }
    fn agent_wait(
        &mut self,
        _target: &str,
        _until: &[AgentStatus],
        _timeout_ms: Option<u64>,
    ) -> Result<AgentInfo> {
        Err(BackendError::unsupported(self.name(), "agent_wait"))
    }
    fn agent_read(&mut self, _target: &str, _source: &str, _lines: u32) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "agent_read"))
    }
    fn agent_send_keys(&mut self, _target: &str, _keys: &[&str]) -> Result<()> {
        Err(BackendError::unsupported(self.name(), "agent_send_keys"))
    }
    /// Subscribe to push events. herdr delivers forward-only (no replay); the
    /// loop's heartbeat poll is the correctness floor (ADR-0001 gap A2,
    /// `bd-event-subscribe-reactor`).
    fn subscribe(&mut self, _filters: &[Subscription]) -> Result<Box<dyn EventStream>> {
        Err(BackendError::unsupported(self.name(), "subscribe"))
    }
}

/// Coordination / mail backend. Default = agent-mail MCP; future = toron
/// (Nostr, in-process). Operation-shaped (the calls flywheel needs), not the
/// raw 38 MCP tools. Each Phase-3 bead graduates a method from `Unsupported`
/// to a real delegation + behavior (`bd-mail-register-on-spawn`, etc.).
pub trait MailBackend: Send + Sync {
    /// Stable backend identifier ("agentmail" | "toron").
    fn name(&self) -> &'static str;

    /// The registered agent identity this backend sends AS / reads the inbox
    /// for (the `sender_name`/`agent_name` in agent-mail calls). Review-fix
    /// F6: the concrete `Mail.name` field is the identity; the trait must
    /// expose it so call sites (inbox, send) route through the factory.
    fn agent_name(&self) -> &str {
        ""
    }

    fn health(&mut self) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "health"))
    }
    fn ensure_project(&mut self, _project_key: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "ensure_project"))
    }
    fn register_agent(
        &mut self,
        _project_key: &str,
        _name: &str,
        _program: &str,
        _model: &str,
        _task: &str,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "register_agent"))
    }
    fn list_agents(&mut self, _project_key: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "list_agents"))
    }
    /// 7 params mirror agent-mail's `send_message` tool surface (separate,
    /// not a struct); loop callers construct them inline.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        _project_key: &str,
        _to: &[String],
        _subject: &str,
        _body: &str,
        _importance: &str,
        _ack_required: bool,
        _thread_id: Option<&str>,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "send"))
    }
    fn reply(
        &mut self,
        _project_key: &str,
        _message_id: u64,
        _body: &str,
        _to: Option<&[String]>,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "reply"))
    }
    fn inbox(&mut self, _project_key: &str, _agent: &str, _filter: &InboxFilter) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "inbox"))
    }
    fn ack(&mut self, _project_key: &str, _agent: &str, _message_id: u64) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "ack"))
    }
    fn mark_read(&mut self, _project_key: &str, _agent: &str, _message_id: u64) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "mark_read"))
    }
    fn reserve(
        &mut self,
        _project_key: &str,
        _agent: &str,
        _paths: &[String],
        _ttl_seconds: u64,
        _exclusive: bool,
        _reason: &str,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "reserve"))
    }
    fn release(&mut self, _project_key: &str, _agent: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "release"))
    }
    fn renew(
        &mut self,
        _project_key: &str,
        _agent: &str,
        _extend_seconds: u64,
        _paths: Option<&[String]>,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "renew"))
    }
    /// Resolve the agent name for a pane from the canonical per-pane identity
    /// file. Carrier-agnostic — the R1 `toron` backend resolves pane identity
    /// natively (ADR-0003 §G9); the agentmail backend delegates to the
    /// agent-mail MCP `resolve_pane_identity` tool, which reads the identity
    /// files the bridge writes (`bd-mail-pane-identity-bridge`, ADR-0001 A4).
    fn resolve_pane_identity(&mut self, _project_key: &str, _pane_id: &str) -> Result<Value> {
        Err(BackendError::unsupported(
            self.name(),
            "resolve_pane_identity",
        ))
    }
    /// Write the per-pane identity file for a spawned agent. Filesystem op,
    /// not an MCP tool — herdr panes are not tmux panes, so agent-mail never
    /// writes identities for them; the bridge does (ADR-0001 A4).
    fn write_pane_identity(
        &mut self,
        _project_key: &str,
        _pane_id: &str,
        _agent_name: &str,
    ) -> Result<Value> {
        Err(BackendError::unsupported(
            self.name(),
            "write_pane_identity",
        ))
    }
    /// Remove the per-pane identity file (pane exited / respawned).
    fn clear_pane_identity(&mut self, _project_key: &str, _pane_id: &str) -> Result<Value> {
        Err(BackendError::unsupported(
            self.name(),
            "clear_pane_identity",
        ))
    }
    fn conflicts(&mut self, _project_key: &str, _agent: &str, _paths: &[String]) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "conflicts"))
    }
    fn slot_acquire(
        &mut self,
        _project_key: &str,
        _agent: &str,
        _slot: &str,
        _exclusive: bool,
        _ttl_seconds: u64,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "slot_acquire"))
    }
    fn slot_release(&mut self, _project_key: &str, _agent: &str, _slot: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "slot_release"))
    }
    fn search(
        &mut self,
        _project_key: &str,
        _query: &str,
        _limit: u32,
        _filter: &SearchFilter,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "search"))
    }
    fn summarize_thread(
        &mut self,
        _project_key: &str,
        _thread_id: &str,
        _llm_mode: bool,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "summarize_thread"))
    }
    fn request_contact(
        &mut self,
        _project_key: &str,
        _from_agent: &str,
        _to_agent: &str,
        _to_project: Option<&str>,
        _reason: &str,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "request_contact"))
    }
    fn respond_contact(
        &mut self,
        _project_key: &str,
        _to_agent: &str,
        _from_agent: &str,
        _accept: bool,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "respond_contact"))
    }
    fn list_contacts(&mut self, _project_key: &str, _agent: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "list_contacts"))
    }
    fn set_contact_policy(
        &mut self,
        _project_key: &str,
        _agent: &str,
        _policy: &str,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "set_contact_policy"))
    }
    fn ensure_product(&mut self, _product_key: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "ensure_product"))
    }
    fn products_link(&mut self, _product_key: &str, _project_key: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "products_link"))
    }
    fn search_product(&mut self, _product_key: &str, _query: &str, _limit: u32) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "search_product"))
    }
    fn inbox_product(&mut self, _product_key: &str, _agent: &str, _limit: u32) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "inbox_product"))
    }
}

#[cfg(test)]
mod trait_contract_tests {
    // RED→GREEN: these references failed to compile until the traits above were
    // defined. The Dummy impls prove object-safety + a complete method surface
    // (only `name()` is overridden; every other method inherits `Unsupported`).

    use super::{BackendError, MailBackend, MultiplexerBackend};

    struct DummyMux;
    impl MultiplexerBackend for DummyMux {
        fn name(&self) -> &'static str {
            "dummy-mux"
        }
    }

    struct DummyMail;
    impl MailBackend for DummyMail {
        fn name(&self) -> &'static str {
            "dummy-mail"
        }
    }

    #[test]
    fn mux_backend_is_object_safe_and_complete() {
        let mut b: Box<dyn MultiplexerBackend> = Box::new(DummyMux);
        assert_eq!(b.name(), "dummy-mux");
        match b.workspace_list() {
            Err(BackendError::Unsupported {
                backend, method, ..
            }) => {
                assert_eq!(backend, "dummy-mux");
                assert_eq!(method, "workspace_list");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn mail_backend_is_object_safe_and_complete() {
        let mut b: Box<dyn MailBackend> = Box::new(DummyMail);
        assert_eq!(b.name(), "dummy-mail");
        match b.health() {
            Err(BackendError::Unsupported {
                backend, method, ..
            }) => {
                assert_eq!(backend, "dummy-mail");
                assert_eq!(method, "health");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    /// Object-safety: the trait can be named as `dyn MultiplexerBackend`.
    #[test]
    fn mux_dyn_name_compiles() {
        fn _assert_dyn(_: &dyn MultiplexerBackend) {}
        _assert_dyn(&DummyMux);
    }
}
