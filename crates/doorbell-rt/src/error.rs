// governed-by: ADR-0012
//! Typed runtime errors (bd-qrmw): library crates carry `thiserror` types,
//! the binary maps them at its edge. Every former `anyhow::bail!` site in
//! `doorbell-rt` has exactly one variant here with an identical message,
//! so worker audit rows (`delivery failed: {e:#}`) render byte-identical.

/// Every failure the doorbell runtime can produce on its own paths.
/// Transport/daemon failures from other crates convert at the call site.
#[derive(Debug, thiserror::Error)]
pub enum RtError {
    /// A sink endpoint is down (mocks + failing test adapters).
    #[error("endpoint down")]
    EndpointDown,
    /// `herdr agent prompt` could not spawn or timed out.
    #[error("herdr prompt spawn/timeout failed")]
    PromptSpawnFailed,
    /// `herdr agent prompt` ran and reported failure on stderr.
    #[error("herdr prompt failed: {0}")]
    PromptFailed(String),
    /// The embedded herdr prompt delivered to no pane.
    #[error("prompt for {0} delivered nowhere")]
    UndeliveredPin(String),
    /// Webhook construction refused: host outside the egress allowlist.
    #[error("webhook host not allowlisted: {0}")]
    NotAllowlisted(String),
    /// Webhook URL has no usable scheme/host (or the socket authority
    /// does not parse).
    #[error("webhook url needs a scheme and a host: {0}")]
    InvalidUrl(String),
    /// Webhook endpoint answered outside 200–299.
    #[error("webhook {host} answered {status}")]
    BadStatus { host: String, status: u16 },
    /// DM sink construction refused: only `svc:` service identities —
    /// the daemon never holds human keys (D6).
    #[error("DM sink refuses non-service identity (daemon never holds human keys)")]
    DmRefused,
    /// The DM transport is down (recording transport in tests).
    #[error("dm transport down")]
    DmDown,
    /// Filesystem failure (journal/wake/heartbeat paths).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encode/decode failure (wake state, webhook render).
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
