//! herdr multiplexer backend (ADR-0005 P1 unified).
//!
//! The herdr backend is now toron-core's `HerdrBackend` implementing the
//! unified `toron_core::backend::Backend` trait — the client was already
//! unified (herdr_shim re-exports toron-core's `Client`), and P1 unified the
//! trait surface. This module re-exports the toron-core impl so flywheel's
//! factory + call sites keep their path (`crate::backend::herdr::HerdrBackend`).
//!
//! governed-by: ADR-0002, ADR-0005

/// The unified herdr backend (toron-core owns the impl).
pub use toron_core::backend::herdr::HerdrBackend;

/// Map a raw `agent.prompt_coded` result to [`PromptLanding`] (review-fix F6).
///
/// A coded error (`agent_prompt_stalled`, `timeout`, …) is landing DATA, not
/// a transport failure. Kept flywheel-side for the parity harness.
pub fn map_prompt_result(
    r: std::result::Result<
        toron_core::backend::herdr::AgentInfo,
        toron_core::backend::BackendError,
    >,
) -> crate::backend::PromptLanding {
    match r {
        Ok(info) => crate::backend::PromptLanding {
            status: Some(info.agent_status),
            code: None,
        },
        Err(e) => crate::backend::PromptLanding {
            status: None,
            code: Some(e.code().as_str().to_string()),
        },
    }
}
