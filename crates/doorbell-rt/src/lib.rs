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
pub mod budget;
pub mod hear;
pub mod host;
pub mod msg;
pub mod policy;
pub mod sensors;
pub mod sinks;
pub mod store;

pub use api::{AskError, Ping, Stats, ask};
pub use brain::{Brain, BrainConfig};
pub use budget::{PinBudget, take_allowed};
pub use hear::HearTracker;
pub use host::{CoreReducer, Effect, Host, MockHost};
pub use msg::{AskKind, AskReply, BrainMsg};
pub use policy::{RestartPolicy, WatchRetry};
pub use sensors::{SensorHandle, bridge, interval_ticker, supervise};
pub use sinks::{MockSink, PromptSink, SendOutcome, SinkManager, Undelivered};
pub use store::FileStore;


/// Single-writer brain inbox capacity (D3: slow brain parks sensors).
pub const BRAIN_CAP: usize = 1024;
/// Per-sink bounded queue capacity (D3: kameo's default 64).
pub const SINK_CAP: usize = 64;
/// Journal envelope version this runtime writes (D4.1).
pub const JOURNAL_V: u64 = 1;
