// governed-by: ADR-0012
//! [`BrainMsg`] — every state change enters the brain as a message (D3).
//! Wall-clock enters only as payloads (replayable); pacing uses monotonic time.

/// All inputs to the single-writer brain. Sensor tasks `tell` these;
/// CLI/API verbs `ask` via [`crate::api::ask`]. Never awaits I/O inside.
#[derive(Debug)]
pub enum BrainMsg {
    /// Plane-S watch NDJSON line (`{kind,id,tags,content,created_at}`).
    Watch(String),
    /// Herdr pane status change for one pin.
    HerdrStatus { pin: String, status: String },
    /// Watch child died; supervisor must restart, never exit (GS-007).
    Died,
    /// 1s sweep beat (`since` RFC3339, `now` unix secs).
    Sweep { since: String, now: u64 },
    /// Role bind drained from the pending queue (live, not snapshot).
    RoleBound { role: String, pin: String },
    /// Captain-bound fact drained live.
    Fact { summary: String },
    /// 60s ack-SLA beat (30-min SLA needs no faster scan).
    AckSlaTick { now: u64 },
    /// Presence projection row (trusted internal edge).
    Presence { pin: String, state: String },
    /// Inbox rows fetched off-loop re-enter here (pipe pattern).
    InboxFetched { pin: String, rows: Vec<serde_json::Value> },
    /// Ack-SLA scan results re-enter here (pipe pattern).
    SlaFired { posts: Vec<String> },
    /// Fault-as-data: sensor/sink death, never fatal to the runtime.
    Degraded { source: String, reason: String },
    /// Ask-verb query; reply travels the oneshot (see [`crate::api`]).
    Ask {
        kind: AskKind,
        reply: tokio::sync::oneshot::Sender<AskReply>,
    },
    /// Graceful stop: drain sinks, journal undelivered, exit.
    Shutdown,
}

/// Ask-verb query kinds (read-only fan-out stays on SSE in Phase 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskKind {
    Stats,
    Digest,
}

/// Ask-verb replies.
#[derive(Debug, Clone)]
pub enum AskReply {
    Stats(crate::api::Stats),
    Digest(Vec<(String, String)>),
}
