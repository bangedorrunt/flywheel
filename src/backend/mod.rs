//! Pluggable backend abstraction — multiplexer (herdr / superlogical / tmux)
//! via the UNIFIED toron-core `Backend` trait (ADR-0005 P1), and the mail
//! fabric (agent-mail MCP / toron) via flywheel-local `MailBackend`.
//!
//! governed-by: ADR-0002, ADR-0005
//!
//! ADR-0005 P1 (bd-p1-unify-y9j): the multiplexer trait surface is UNIFIED in
//! toron-core (`toron_core::backend::Backend`). flywheel no longer defines a
//! local `MultiplexerBackend` trait — it re-exports the unified one:
//! `pub use toron_core::backend::Backend as MultiplexerBackend;`. Call sites
//! keep the name; the trait is sync, `&self`, object-safe, Unsupported-open.
//! The herdr *client* was already unified (herdr_shim re-exports toron-core).
//!
//! `MailBackend` is EXPLICITLY DEFERRED (ADR-0005 P1.1 decision #7): a
//! separate coordination domain (~30 AM-parity ops) that stays flywheel-local
//! and keeps flywheel's `BackendError` (machine-message style with
//! `Unsupported{backend,method,reason}`). P1 does NOT merge it into the
//! unified `Backend`; a future phase may move it to toron-core.
//!
//! Design (inherited from the unified trait + MailBackend): every method
//! except `name()` has a default body returning an Unsupported error. Real
//! impls override what they support (ADR-0002 §6 — open-to-extension).

// TODO(bd-p2-cli-wn8): re-evaluate this allow when the catalog CLI lands.
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
pub mod toron;

use serde_json::Value;

/// The unified multiplexer backend trait (ADR-0005 P1): sync, `&self`,
/// object-safe, Unsupported-open. The ONE trait source lives in toron-core;
/// this re-export keeps flywheel call sites on the historical name.
pub use toron_core::backend::Backend as MultiplexerBackend;
/// Coded `agent.prompt` landing data (unified in toron-core).
pub use toron_core::backend::PromptLanding;

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

impl From<crate::herdr_shim::HerdrError> for BackendError {
    /// Preserve herdr's typed error code as an `Other` with a lossless
    /// anyhow context. (Backend callers handle `BackendError::Unsupported`;
    /// transport errors stay opaque in `Other`.)
    fn from(e: crate::herdr_shim::HerdrError) -> Self {
        Self::Other(anyhow::anyhow!("herdr error {}: {}", e.code, e.message))
    }
}

impl From<toron_core::backend::BackendError> for BackendError {
    /// The toron-core herdr client's typed error (ADR-0003 Decision 3A,
    /// bd-o8j.2): classification on `code`, message in the `Other` context.
    fn from(e: toron_core::backend::BackendError) -> Self {
        Self::Other(anyhow::anyhow!(
            "herdr error {}: {}",
            e.code().as_str(),
            e.message
        ))
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
}

/// Type alias so trait signatures read `Result<T>` and real impls get
/// `anyhow::Error -> BackendError` coercion for free via the `From` impl.
/// (MailBackend-only in P1 — the multiplexer uses toron-core's `BackendError`.)
pub type Result<T> = std::result::Result<T, BackendError>;

/// Coordination / mail backend. Default = agent-mail MCP; future = toron
/// (Nostr, in-process). Operation-shaped (the calls flywheel needs), not the
/// raw 38 MCP tools. Each Phase-3 bead graduates a method from `Unsupported`
/// to a real delegation + behavior (`bd-mail-register-on-spawn`, etc.).
///
/// Receivers are `&self` (c3b.2): no impl mutates state — backends use
/// interior mutability (`OnceLock`/`AtomicU64` in the MCP client, the shared
/// mailbox behind `Arc`). This makes `Arc<dyn MailBackend>` shareable across
/// loop iterations without a serializing Mutex.
pub trait MailBackend: Send + Sync {
    /// Stable backend identifier ("agentmail" | "toron").
    fn name(&self) -> &'static str;

    /// The registered agent identity this backend sends AS / reads the inbox
    /// for (the `sender_name`/`agent_name` in agent-mail calls). Review-fix
    /// F6: the concrete `Mail.name` field is the identity; the trait must
    /// expose it so call sites (inbox, send) route through the factory.
    ///
    /// REQUIRED (no default): a `""` default would let a backend silently
    /// send/read mail as an EMPTY identity — a silent-wrong-behavior trap.
    /// Both real impls override it; a new impl must too (compile-enforced).
    fn agent_name(&self) -> &str;

    fn health(&self) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "health"))
    }
    fn ensure_project(&self, _project_key: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "ensure_project"))
    }
    fn register_agent(
        &self,
        _project_key: &str,
        _name: &str,
        _program: &str,
        _model: &str,
        _task: &str,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "register_agent"))
    }
    fn list_agents(&self, _project_key: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "list_agents"))
    }
    /// 7 params mirror agent-mail's `send_message` tool surface (separate,
    /// not a struct); loop callers construct them inline.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &self,
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
        &self,
        _project_key: &str,
        _message_id: &str,
        _body: &str,
        _to: Option<&[String]>,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "reply"))
    }
    fn inbox(&self, _project_key: &str, _agent: &str, _filter: &InboxFilter) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "inbox"))
    }
    fn ack(&self, _project_key: &str, _agent: &str, _message_id: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "ack"))
    }
    fn mark_read(&self, _project_key: &str, _agent: &str, _message_id: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "mark_read"))
    }
    fn reserve(
        &self,
        _project_key: &str,
        _agent: &str,
        _paths: &[String],
        _ttl_seconds: u64,
        _exclusive: bool,
        _reason: &str,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "reserve"))
    }
    fn release(&self, _project_key: &str, _agent: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "release"))
    }
    fn renew(
        &self,
        _project_key: &str,
        _agent: &str,
        _extend_seconds: u64,
        _paths: Option<&[String]>,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "renew"))
    }
    /// Operator force-release of a stale reservation (`force_release_file_reservation`).
    /// Non-operator surface per ADR-0003 V6 (the destructive hint is the
    /// caller's responsibility — it is NOT gated behind the excluded
    /// operator-only macro/guard/create identities).
    fn force_release_file_reservation(
        &self,
        _project_key: &str,
        _agent: &str,
        _file_reservation_id: u64,
        _note: Option<&str>,
        _notify_previous: bool,
    ) -> Result<Value> {
        Err(BackendError::unsupported(
            self.name(),
            "force_release_file_reservation",
        ))
    }
    /// Resolve the agent name for a pane from the canonical per-pane identity
    /// file. Carrier-agnostic — the R1 `toron` backend resolves pane identity
    /// natively (ADR-0003 §G9); the agentmail backend delegates to the
    /// agent-mail MCP `resolve_pane_identity` tool, which reads the identity
    /// files the bridge writes (`bd-mail-pane-identity-bridge`, ADR-0001 A4).
    fn resolve_pane_identity(&self, _project_key: &str, _pane_id: &str) -> Result<Value> {
        Err(BackendError::unsupported(
            self.name(),
            "resolve_pane_identity",
        ))
    }
    /// Write the per-pane identity file for a spawned agent. Filesystem op,
    /// not an MCP tool — herdr panes are not tmux panes, so agent-mail never
    /// writes identities for them; the bridge does (ADR-0001 A4).
    fn write_pane_identity(
        &self,
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
    fn clear_pane_identity(&self, _project_key: &str, _pane_id: &str) -> Result<Value> {
        Err(BackendError::unsupported(
            self.name(),
            "clear_pane_identity",
        ))
    }
    /// Prune identities whose panes are absent from the live pane list
    /// (`cleanup_pane_identities`; ADR-0003 V6 surface restore).
    fn cleanup_pane_identities(&self, _project_key: &str) -> Result<Value> {
        Err(BackendError::unsupported(
            self.name(),
            "cleanup_pane_identities",
        ))
    }
    fn conflicts(&self, _project_key: &str, _agent: &str, _paths: &[String]) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "conflicts"))
    }
    fn slot_acquire(
        &self,
        _project_key: &str,
        _agent: &str,
        _slot: &str,
        _exclusive: bool,
        _ttl_seconds: u64,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "slot_acquire"))
    }
    fn slot_release(&self, _project_key: &str, _agent: &str, _slot: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "slot_release"))
    }
    /// Extend an active build-slot lease's TTL (`renew_build_slot`). Idempotent
    /// and safe to over-call (ADR-0003 V6 restores the full non-operator
    /// surface; the concrete `Mail` facade already wraps it as `renew_slot`).
    fn slot_renew(
        &self,
        _project_key: &str,
        _agent: &str,
        _slot: &str,
        _extend_seconds: u64,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "slot_renew"))
    }
    fn search(
        &self,
        _project_key: &str,
        _query: &str,
        _limit: u32,
        _filter: &SearchFilter,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "search"))
    }
    fn summarize_thread(
        &self,
        _project_key: &str,
        _thread_id: &str,
        _llm_mode: bool,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "summarize_thread"))
    }
    /// Summarize a thread across a product's member projects
    /// (`summarize_thread_product`; ADR-0003 V6 surface restore).
    fn summarize_thread_product(
        &self,
        _product_key: &str,
        _thread_id: &str,
        _llm_mode: bool,
    ) -> Result<Value> {
        Err(BackendError::unsupported(
            self.name(),
            "summarize_thread_product",
        ))
    }
    fn request_contact(
        &self,
        _project_key: &str,
        _from_agent: &str,
        _to_agent: &str,
        _to_project: Option<&str>,
        _reason: &str,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "request_contact"))
    }
    fn respond_contact(
        &self,
        _project_key: &str,
        _to_agent: &str,
        _from_agent: &str,
        _accept: bool,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "respond_contact"))
    }
    fn list_contacts(&self, _project_key: &str, _agent: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "list_contacts"))
    }
    fn set_contact_policy(
        &self,
        _project_key: &str,
        _agent: &str,
        _policy: &str,
    ) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "set_contact_policy"))
    }
    fn ensure_product(&self, _product_key: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "ensure_product"))
    }
    fn products_link(&self, _product_key: &str, _project_key: &str) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "products_link"))
    }
    fn search_product(&self, _product_key: &str, _query: &str, _limit: u32) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "search_product"))
    }
    fn inbox_product(&self, _product_key: &str, _agent: &str, _limit: u32) -> Result<Value> {
        Err(BackendError::unsupported(self.name(), "inbox_product"))
    }
}

#[cfg(test)]
mod trait_contract_tests {
    // The unified `MultiplexerBackend` (= toron-core `Backend`) is proven
    // object-safe + Unsupported-open by toron-core's own tests. Here we keep
    // the MailBackend (deferred, flywheel-local) contract check.

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
        fn agent_name(&self) -> &str {
            "dummy-agent"
        }
    }

    #[test]
    fn unified_mux_is_object_safe_and_unsupported_open() {
        // `&self` receiver (unified trait): no `mut` needed.
        let b: Box<dyn MultiplexerBackend> = Box::new(DummyMux);
        assert_eq!(b.name(), "dummy-mux");
        let err = b.workspace_list().unwrap_err();
        assert_eq!(
            err.code(),
            &toron_core::backend::error::BackendErrorCode::Unsupported
        );
        assert!(err.message.contains("dummy-mux"));
        assert!(err.message.contains("workspace_list"));
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

    /// Object-safety: the unified trait can be named as `dyn MultiplexerBackend`.
    #[test]
    fn mux_dyn_name_compiles() {
        fn _assert_dyn(_: &dyn MultiplexerBackend) {}
        _assert_dyn(&DummyMux);
    }
}
