// governed-by: ADR-0012
//! Ask verbs (D8): request + reply channel + timeout, errors as typed
//! values — the CLI renders "daemon wedged" instead of hanging.

use crate::msg::{AskKind, AskReply, BrainMsg};
use std::collections::BTreeMap;
use tokio::sync::{mpsc, oneshot};

/// Ask taxonomy (kameo's `SendError` adapted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskError {
    /// Brain alive but too slow (timeout).
    Busy,
    /// Brain gone (channel closed).
    Absent,
    /// Brain alive but wedged (last sweep too old — reported by stats).
    Wedged { last_sweep_age_s: u64 },
    /// Verb refused by the authz matrix (D6; enforced at the API edge).
    Refused(String),
}

impl std::fmt::Display for AskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AskError::Busy => write!(f, "daemon busy (ask timed out)"),
            AskError::Absent => write!(f, "daemon absent (brain channel closed)"),
            AskError::Wedged { last_sweep_age_s } => {
                write!(f, "daemon wedged (last sweep {last_sweep_age_s}s ago)")
            }
            AskError::Refused(reason) => write!(f, "refused: {reason}"),
        }
    }
}

/// Counters that make the wake SLA measurable (D3 shed counts, D4
/// reconcile counts ride here in later phases).
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub rings_by_class: BTreeMap<String, u64>,
    pub injects: u64,
    pub drops: u64,
    pub sheds: u64,
    pub deaf: u64,
    pub digest_batches: u64,
    pub sweep_lag_s: u64,
    pub degraded: u64,
}

/// Version handshake payload (D8): skew refuses loudly, never mis-renders.
#[derive(Debug, Clone)]
pub struct Ping {
    pub protocol: &'static str,
    pub core_v: &'static str,
    pub journal_v: u64,
}

pub const PING: Ping = Ping {
    protocol: "doorbell.v1",
    core_v: "doorbell-core.v1",
    journal_v: crate::JOURNAL_V,
};

/// Ask the brain; timeout is the caller's SLA, not the brain's.
pub async fn ask(
    brain_tx: &mpsc::Sender<BrainMsg>,
    kind: AskKind,
    timeout: std::time::Duration,
) -> Result<AskReply, AskError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    brain_tx
        .send(BrainMsg::Ask {
            kind,
            reply: reply_tx,
        })
        .await
        .map_err(|_| AskError::Absent)?;
    tokio::time::timeout(timeout, reply_rx)
        .await
        .map_err(|_| AskError::Busy)?
        .map_err(|_| AskError::Absent)
}
