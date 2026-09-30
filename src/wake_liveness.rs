//! GS-062 wake-pipe classifier — shared implementation in
//! `doorbell_rt::wake_liveness` (also used by operator doorbell `/health`).
//!
//! This is a re-export shim: the classifier and its tests live with the
//! implementation in `doorbell-rt`. Keep it that way — a second copy here
//! would prove the copy, not the implementation.
//!
//! Gated on `serve` because its only consumer is `serve_async`'s /health
//! handler; a `serve`-off build has no reader and would warn on the glob.
#[cfg(feature = "serve")]
pub use doorbell_rt::wake_liveness::*;
