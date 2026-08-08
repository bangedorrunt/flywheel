//! superlogical multiplexer backend — STUB.
//!
//! governed-by: ADR-0002  ·  implements: `MultiplexerBackend`
//!
//! superlogical (superlogical.com — Mitchell Hashimoto's team) is a terminal
//! multiplexer in pre-release (beta signup). This stub reserves the
//! `FLYWHEEL_BACKEND=superlogical` selection: setting it today yields a clear
//! "not yet released" error on every operation (the trait defaults return
//! `BackendError::Unsupported` naming this backend), NOT a silent fallback to
//! herdr. When superlogical ships its SDK/protocol (ADR-0002 R2), this file
//! graduates from stub to real impl — the `name()` + trait surface stay, only
//! the method bodies change.
//!
//! gated by: cargo feature `backend-superlogical` (off by default).

use crate::backend::MultiplexerBackend;
/// Stub superlogical backend. `name()` is the only override — every trait
/// method inherits the default `BackendError::Unsupported` (which names this
/// backend), so any use is an honest "not yet released" error.
pub struct SuperlogicalBackend;

impl SuperlogicalBackend {
    /// Why operations fail today (surfaced by the factory's error path too).
    pub const REASON: &'static str =
        "superlogical is not yet released (superlogical.com beta); set FLYWHEEL_BACKEND=herdr";
}

impl MultiplexerBackend for SuperlogicalBackend {
    fn name(&self) -> &'static str {
        "superlogical"
    }
}
