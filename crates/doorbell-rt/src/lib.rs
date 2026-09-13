// governed-by: ADR-0012
//! `doorbell-rt` — the doorbell runtime library (ADR-0012 D1/D3, Phase 1).
//!
//! One implementation, two hosts: flywheel embeds this crate, or the
//! `doorbell` daemon binary hosts it. The [`Brain`] is the single writer —
//! sensors `tell` [`BrainMsg`] into one bounded channel, sinks take bounded
//! prompt intents, degradation arrives as data. No tokio actor framework;
//! plain tasks + `mpsc`/`oneshot` (kameo concepts, Appendix A).
pub mod api;
pub mod brain;
pub mod file_host;
pub mod hear;
pub mod host;
pub mod lease;
pub mod msg;
pub mod policy;
pub mod pollers;
pub mod sensors;
pub mod sinks;
pub mod store;
pub mod subscribe;
pub mod watchbeat;
pub use api::{AskError, Ping, Stats, ask};
pub use brain::{Brain, BrainConfig, RunReport};
pub use doorbell_core::registry_key;
pub use doorbell_core::{PinBudget, take_allowed, take_allowed_like};
pub use file_host::{
    FileHost, FileHostConfig, WATCH_KINDS, ensure_daemon_gen, presence_path, resolve_registry_file,
    toron_data_root,
};
pub use hear::HearTracker;
pub use host::{CoreReducer, Effect, Host, MockHost};
pub use lease::{Deference, OwnerLease, acquire_lease, deference_decision, lease_stale};
pub use msg::{AskKind, AskReply, BrainMsg};
pub use policy::{RestartPolicy, WatchRetry};
pub use sensors::{SensorHandle, bridge, interval_ticker, supervise};
pub use sinks::{CliPromptSink, MockSink, PromptSink, SendOutcome, SinkManager, Undelivered};
pub use store::{FileStore, Fold, TornTail};
pub use subscribe::{
    Actor, Budget, Filter, Identity, Refusal, Scope, SinkKind, Subscription, SubscriptionStore,
    authorize_subscribe,
};

/// Journal envelope version this runtime writes (D4.1: v2 adds subscriber
/// scoping + budget rows; readers tolerate unknown fields, refuse unknown
/// versions loudly).
pub const JOURNAL_V: u64 = 2;
/// Single-writer brain inbox capacity (D3: slow brain parks sensors).
pub const BRAIN_CAP: usize = 1024;
/// Per-sink bounded queue capacity (D3: kameo's default 64).
pub const SINK_CAP: usize = 64;
