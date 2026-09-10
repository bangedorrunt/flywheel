// governed-by: ADR-0012
//! Doorbell Elm tick shim (ADR-0012 Phase 0): the reducer moved verbatim into
//! `doorbell-core`. This module re-exports it so the live path
//! (`workspace::tick`, journal, tests) keeps its paths with zero behavior change.
pub use doorbell_core::{tick, DoorbellAction, DoorbellModel, DoorbellMsg};
