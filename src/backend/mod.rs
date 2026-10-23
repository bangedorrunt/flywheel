//! Pluggable backend abstraction — multiplexer (herdr / superlogical)
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
//! as `MailSurface`; `ToronMailBackend` (toron.rs) implements it.
//! `BackendError` now serves the multiplexer + factory only.
//!
//! Design (inherited from the unified trait): every `MultiplexerBackend`
//! method except `name()` has a default body returning an Unsupported error.
//! Real impls override what they support (ADR-0002 §6 — open-to-extension).

pub mod factory;
pub mod herdr;
#[cfg(test)]
mod parity;
#[cfg(feature = "backend-superlogical")]
pub mod rex_map;
#[cfg(feature = "backend-superlogical")]
pub mod superlogical;
#[cfg(feature = "mail-toron")]
pub mod toron;
pub mod wire;

/// The unified multiplexer backend trait (ADR-0005 P1): sync, `&self`,
/// object-safe, Unsupported-open. The ONE trait source lives in toron-core;
/// this re-export keeps flywheel call sites on the historical name.
pub use toron_core::backend::Backend as MultiplexerBackend;
/// Coded `agent.prompt` landing data (unified in toron-core). Reached only
/// from the `__send` parity mapper (`herdr::map_prompt_result`), which is
/// itself test-only here, so the name ships with that caller.
#[cfg(test)]
pub use toron_core::backend::PromptLanding;

// ===========================================================================
// Mail coordination surface (ADR-0007 D2 / R1, se2.3) — RE-EXPORTED from
// toron-core, never redefined here. The local `MailBackend` trait was
// deleted with se2.3; flywheel consumes `MailSurface` and its typed DTOs.
// ===========================================================================
#[allow(unused_imports)]
pub use toron_core::index::slots::{BuildSlot, GrantedSlot, SlotConflict};
#[allow(unused_imports)]
pub use toron_core::mailbox::build_slots::AcquireBuildSlotResult;
#[allow(unused_imports)]
pub use toron_core::mailbox::contacts::ContactLink;
pub use toron_core::mailbox::error::MailError;
#[allow(unused_imports)]
pub use toron_core::mailbox::ids::MessageId;
#[allow(unused_imports)]
pub use toron_core::mailbox::infra::{HealthReport, ProjectState};
#[allow(unused_imports)]
pub use toron_core::mailbox::product::ProductInfo;
#[allow(unused_imports)]
pub use toron_core::mailbox::reservations::{
    AcquireResult, ConflictReport, GrantedReservation, PathConflict, ReleaseResult, ReleasedLease,
    RenewResult,
};
#[allow(unused_imports)]
pub use toron_core::mailbox::search::ThreadSummary;
#[allow(unused_imports)]
pub use toron_core::mailbox::surface::{
    ContactRequest, ContactResponse, DeliveryLine, InboxQuery, MailItem, MailSurface, ProductHit,
    ReceiptOp, ReceiptOutcome, ReplyMail, SearchQuery, SendMail, SendReceipt,
};
#[allow(unused_imports)]
pub use toron_core::mailbox::{AgentProfile, RegisteredAgent, WhoIs};

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
/// (in-process ops are pure async; adapters are sync-HTTP), so this is safe
/// **from a sync context**. A caller already driving async tasks (a durable
/// leaf body runs on the loop's tokio worker) must use
/// [`run_mail_off_runtime`] instead.
pub fn run_mail<F: std::future::Future>(f: F) -> F::Output {
    crate::mail_shim::block_on(f)
}

/// The async-safe twin of [`run_mail`]: run one synchronous mail block (which
/// `block_on`s the shared runtime inside) on a BLOCKING thread.
///
/// bd-swarm-loop-durable-panic-atlo: calling [`run_mail`] from an async
/// context that is already driving tasks panics with "Cannot start a runtime
/// from within a runtime" — the renew-filtered durable leaf did exactly that
/// once per poll on the loop's tokio worker. `spawn_blocking` moves the whole
/// block off the scheduler first; there the shared-runtime `block_on` is the
/// documented thread→runtime bridge, the same F8 pattern
/// `backend/toron.rs::run_facade` uses for the async surface methods.
pub async fn run_mail_off_runtime<F, T>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("mail blocking task join failed: {e}"))?
}

/// The sync twin of [`run_mail_off_runtime`] for callers that BORROW (a
/// `&mut self` host method, a `&Config`): run `f` on a scoped plain thread
/// when the caller is inside a tokio runtime, inline otherwise. A plain
/// thread has no runtime context, so the shared-runtime `block_on` inside
/// `f` is the same thread->runtime bridge the async helper uses.
///
/// The embedded doorbell's `Host` methods are sync but the brain drives them
/// from its runtime thread; every mail-touching one goes through this.
pub fn run_mail_off_runtime_sync<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    if tokio::runtime::Handle::try_current().is_err() {
        return f();
    }
    std::thread::scope(|s| match s.spawn(f).join() {
        Ok(v) => v,
        Err(p) => std::panic::resume_unwind(p),
    })
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
            Self::Unsupported {
                backend,
                method,
                reason,
            } => write!(
                f,
                "backend '{backend}' does not implement '{method}': {reason}"
            ),
            Self::Other(e) => write!(f, "{e}"),
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

impl BackendError {}

/// Type alias for multiplexer + factory errors (the mail surface returns
/// toron-core's `MailError` directly — no double error taxonomy, ADR-0007 F9).
pub type Result<T> = std::result::Result<T, BackendError>;

impl From<MailError> for BackendError {
    fn from(e: MailError) -> Self {
        Self::Other(anyhow::anyhow!("{e}"))
    }
}

#[cfg(test)]
mod off_runtime_tests {
    use super::run_mail_off_runtime;

    /// The bridge must let the renew leaf's exact call shape (`run_mail`'s
    /// shared-runtime `block_on`) run from inside a runtime without the
    /// nested-runtime panic: prove it end-to-end on a real multi-thread
    /// runtime. Inline (the pre-fix leaf) this test panics "Cannot start a
    /// runtime from within a runtime" — verified red by calling the closure
    /// directly before this helper existed.
    #[test]
    fn run_mail_off_runtime_bridges_without_nesting() {
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        let v = rt
            .block_on(async {
                run_mail_off_runtime(|| Ok(crate::mail_shim::block_on(async { 7i32 }))).await
            })
            .expect("the bridge must not nest block_on on the caller's runtime");
        assert_eq!(v, 7);
    }

    /// The sync twin: same call shape from a borrowing caller inside a
    /// runtime, and a plain pass-through outside one.
    #[test]
    fn run_mail_off_runtime_sync_bridges_inside_a_runtime_and_passes_through_outside() {
        let outside =
            super::run_mail_off_runtime_sync(|| crate::mail_shim::block_on(async { 3i32 }));
        assert_eq!(outside, 3);
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        let borrowed = String::from("x");
        let inside = rt.block_on(async {
            super::run_mail_off_runtime_sync(|| {
                (borrowed.len(), crate::mail_shim::block_on(async { 4i32 }))
            })
        });
        assert_eq!(inside, (1, 4));
    }

    /// The hazard the bridge exists for, pinned as a canary: a direct
    /// shared-runtime `block_on` from inside another runtime panics. If a
    /// future tokio ever permits the nesting, this test goes red and tells
    /// us the bridge (and the leaf's shape) can be revisited.
    #[test]
    fn inline_shared_runtime_block_on_panics_inside_a_runtime() {
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(async { crate::mail_shim::block_on(async { 1i32 }) })
        }));
        assert!(
            res.is_err(),
            "nested block_on was expected to panic; the async-safe bridge is only necessary while it does"
        );
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
