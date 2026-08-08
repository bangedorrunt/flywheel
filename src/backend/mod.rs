//! Pluggable backend abstraction — multiplexer (herdr / superlogical / tmux)
//! via the UNIFIED toron-core `Backend` trait (ADR-0005 P1), and the mail
//! fabric (toron / agent-mail MCP) via the UNIFIED toron-core `MailSurface`
//! trait (ADR-0007 D2 / R1, consumed by flywheel in bead bd-adr7-mail-adapt-se2.3).
//!
//! governed-by: ADR-0002, ADR-0005, ADR-0007
//!
//! ADR-0005 P1 (bd-p1-unify-y9j): the multiplexer trait surface is UNIFIED in
//! toron-core (`toron_core::backend::Backend`). flywheel no longer defines a
//! local `MultiplexerBackend` trait — it re-exports the unified one:
//! `pub use toron_core::backend::Backend as MultiplexerBackend;`. Call sites
//! keep the name; the trait is sync, `&self`, object-safe, Unsupported-open.
//! The herdr *client* was already unified (herdr_shim re-exports toron-core).
//!
//! ADR-0007 D2 (se2.3): the flywheel-local `MailBackend` trait is DELETED
//! (not dual-lived). The coordination-fabric trait is
//! `toron_core::mailbox::surface::MailSurface` — async, `&self`, `MessageId`
//! string ids end-to-end, machine-coded `MailError`. flywheel re-exports it
//! as `MailSurface`; `ToronMailBackend` (toron.rs) and `AgentMailBackend`
//! (agentmail.rs) implement it. The AM adapter converts u64 ↔ String ONLY at
//! the MCP JSON edge, with a parse error (never `unwrap_or(0)` — se2.2).
//! `BackendError` now serves the multiplexer + factory only.
//!
//! Design (inherited from the unified trait): every `MultiplexerBackend`
//! method except `name()` has a default body returning an Unsupported error.
//! Real impls override what they support (ADR-0002 §6 — open-to-extension).

// TODO(bd-p2-cli-wn8): re-evaluate this allow when the catalog CLI lands.
#![allow(dead_code)]

pub mod agentmail;
pub mod factory;
pub mod herdr;
pub mod wire;
#[cfg(test)]
mod parity;
#[cfg(feature = "backend-superlogical")]
pub mod superlogical;
#[cfg(feature = "backend-tmux")]
pub mod tmux;
#[cfg(feature = "mail-toron")]
pub mod toron;

/// The unified multiplexer backend trait (ADR-0005 P1): sync, `&self`,
/// object-safe, Unsupported-open. The ONE trait source lives in toron-core;
/// this re-export keeps flywheel call sites on the historical name.
pub use toron_core::backend::Backend as MultiplexerBackend;
/// Coded `agent.prompt` landing data (unified in toron-core).
pub use toron_core::backend::PromptLanding;

// ===========================================================================
// Mail coordination surface (ADR-0007 D2 / R1, se2.3) — RE-EXPORTED from
// toron-core, never redefined here. The local `MailBackend` trait was
// deleted with se2.3; flywheel consumes `MailSurface` and its typed DTOs.
// ===========================================================================
pub use toron_core::mailbox::surface::{
    ContactRequest, ContactResponse, DeliveryLine, InboxQuery, MailItem, MailSurface,
    ProductHit, ReceiptOp, ReceiptOutcome, ReplyMail, SearchQuery, SendMail, SendReceipt,
};
pub use toron_core::mailbox::error::MailError;
pub use toron_core::mailbox::ids::MessageId;
pub use toron_core::mailbox::{AgentProfile, RegisteredAgent, WhoIs};
#[allow(unused_imports)]
pub use toron_core::mailbox::infra::{HealthReport, ProjectState};
pub use toron_core::mailbox::contacts::ContactLink;
pub use toron_core::mailbox::search::ThreadSummary;
pub use toron_core::mailbox::product::ProductInfo;
#[allow(unused_imports)]
pub use toron_core::mailbox::reservations::{
    AcquireResult, ConflictReport, GrantedReservation, PathConflict, ReleaseResult, RenewResult,
};
pub use toron_core::mailbox::build_slots::AcquireBuildSlotResult;
pub use toron_core::mailbox::unwrap::ContactPolicy;
#[allow(unused_imports)]
pub use toron_core::index::slots::{BuildSlot, GrantedSlot, SlotConflict};

// Factory entry points — consumers (the durable-loop leaf handlers in main.rs)
// call `backend::multiplexer_backend(&cfg)` / `backend::mail_backend(&cfg)`;
// `flywheel backend list/status` (bd-cli-backend-subcommand) uses the resolve_
// and compiled_backends helpers.
pub use factory::{
    compiled_backends, mail_backend, multiplexer_backend, resolve_backend_name, resolve_mail_name,
};

/// Drive one `MailSurface` future from sync flywheel dispatch (the durable
/// loop is sync; each op is a short-lived block_on on the shared runtime —
/// c3b.1). The surface impls never nest `block_on` on that runtime
/// (in-process ops are pure async; adapters are sync-HTTP), so this is safe.
pub fn run_mail<F: std::future::Future>(f: F) -> F::Output {
    crate::mail_shim::block_on(f)
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

/// Type alias for multiplexer + factory errors (the mail surface returns
/// toron-core's `MailError` directly — no double error taxonomy, ADR-0007 F9).
pub type Result<T> = std::result::Result<T, BackendError>;

impl From<MailError> for BackendError {
    fn from(e: MailError) -> Self {
        Self::Other(anyhow::anyhow!("{e}"))
    }
}

#[cfg(test)]
mod trait_contract_tests {
    // The unified `MultiplexerBackend` (= toron-core `Backend`) is proven
    // object-safe + Unsupported-open by toron-core's own tests; `MailSurface`
    // (= toron-core `mailbox::surface::MailSurface`) likewise. Here we keep
    // the flywheel-side contract checks (factory constructability + re-export
    // dyn-compile).

    use super::MultiplexerBackend;

    struct DummyMux;
    impl MultiplexerBackend for DummyMux {
        fn name(&self) -> &'static str {
            "dummy-mux"
        }
    }

    /// Object-safety: the unified trait can be named as `dyn MultiplexerBackend`.
    #[test]
    fn mux_dyn_name_compiles() {
        fn _assert_dyn(_: &dyn MultiplexerBackend) {}
        _assert_dyn(&DummyMux);
    }
}
