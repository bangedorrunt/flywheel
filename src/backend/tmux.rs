//! tmux multiplexer backend — STUB.
//!
//! governed-by: ADR-0002  ·  implements: `MultiplexerBackend`
//!
//! tmux is the legacy multiplexer, on hold per toron ADR-0003 §architecture
//! layer 5 ("tmux returns Unsupported until un-held"). This stub reserves the
//! `FLYWHEEL_BACKEND=tmux` selection: setting it today yields a clear
//! "on hold" error on every operation, NOT a silent fallback to herdr.
//!
//! gated by: cargo feature `backend-tmux` (off by default).

use crate::backend::MultiplexerBackend;

/// Stub tmux backend. `name()` is the only override — every trait method
/// inherits the default `BackendError::Unsupported` (naming this backend).
pub struct TmuxBackend;

impl TmuxBackend {
    /// Why operations fail today (toron ADR-0003 holds the tmux backend).
    pub const REASON: &'static str = "tmux backend is on hold per toron ADR-0003 (un-held is a future toron-core impl); set FLYWHEEL_BACKEND=herdr";
}

impl MultiplexerBackend for TmuxBackend {
    fn name(&self) -> &'static str {
        "tmux"
    }
}
