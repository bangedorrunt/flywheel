//! toron mail backend — STUB.
//!
//! governed-by: ADR-0002  ·  implements: `MailBackend`
//!
//! toron (`~/workspace/toron`, ADR-0003) is a Nostr-native mailbox with 100%
//! parity to mcp_agent_mail_rust, designed to be EMBEDDED in flywheel
//! in-process (ADR-0003 Decision 3A). Until toron-core ships as an embeddable
//! crate (ADR-0002 R1), this stub reserves the `FLYWHEEL_MAIL=toron`
//! selection: setting it today yields a clear "not yet embedded" error on
//! every operation, NOT a silent fallback to agentmail. At R1, `ToronMailBackend`
//! graduates to a real in-process impl (constructs `toron_core::Mailbox`).
//!
//! gated by: cargo feature `mail-toron` (off by default).

use crate::backend::{BackendError, MailBackend};

/// Stub toron mail backend. `name()` is the only override — every trait
/// method inherits the default `BackendError::Unsupported` (naming this
/// backend).
pub struct ToronMailBackend;

impl ToronMailBackend {
    /// Why operations fail today (toron-core not yet embeddable).
    pub const REASON: &'static str =
        "toron-core not yet embedded (toron ADR-0003 R1); set FLYWHEEL_MAIL=agentmail";
}

impl MailBackend for ToronMailBackend {
    fn name(&self) -> &'static str {
        "toron"
    }
}

/// Convenience: the whole-backend Unsupported error the factory surfaces.
pub fn stub_error() -> BackendError {
    BackendError::stub("toron", ToronMailBackend::REASON)
}
